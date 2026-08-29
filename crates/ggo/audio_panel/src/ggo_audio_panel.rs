//! GGO Audio: a center-pane tab per `.wav` / `.ogg` / `.adp` that lets the
//! studio HEAR a musician's file, hear what the hardware will make of it,
//! pick the baked rate, and write the `.adp` the cart ships.
//!
//! There is deliberately no synthesis, sequencer, or PSG/ADSR/pan authoring
//! here: musicians deliver finished wav/ogg, and emerald's runtime never
//! programs those APU dimensions, so nothing authored in them could ship.
//! What the editor owns is the trip from a delivered file to a cart asset
//! -- `ggo_audio` does the codec work, this panel is the surface.
//!
//! **Import, not sidecar.** The rate knob is editor-side: Import bakes at
//! the chosen rate and writes `assets/<stem>.adp`, which `emd pack-ggo`
//! copies verbatim (`AssetKind::Adp`). Emerald is not touched and knows
//! nothing about this editor. Dropping a raw `.wav`/`.ogg` under `assets/`
//! keeps working at emerald's default rate for anyone who doesn't care.
//!
//! **Baked preview is the real thing.** `preview.rs` runs the blob through
//! a standalone `ggo_emu_core::apu::Apu` -- 4-bit ADPCM, the 4.12 phase
//! step, the 32 kHz mix -- into the emulator pane's cpal ring. Source is
//! the decoded PCM as delivered. A/B between them is the whole point of
//! the rate picker.

mod audio_item;
mod load;
mod preview;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use editor::Editor;
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled, Task,
    WeakEntity, Window, actions, div, point, px, size,
};
use project::ProjectPath;
use ui::prelude::*;
use ui::{Checkbox, ContextMenu, DropdownMenu, ToggleState};
use workspace::Workspace;

use ggo_audio::Decoded;
use ggo_emu_panel::audio::AudioStatus;

pub use audio_item::AudioItem;
use load::Loaded;
use preview::{Preview, Spec};

actions!(
    ggo_audio,
    [
        /// Plays or stops the audio preview.
        PlayStop,
        /// Toggles looping playback.
        ToggleLoop,
    ]
);

const KEY_CONTEXT: &str = "GgoAudioPanel";

/// The extensions this tab claims from the file explorer: the two source
/// containers emerald bakes, and the baked form itself.
const AUDIO_EXTS: [&str; 3] = ["wav", "ogg", "adp"];

const WAVEFORM_HEIGHT_PX: f32 = 160.0;
/// The playhead redraw cadence while a preview runs.
const PLAYHEAD_TICK: Duration = Duration::from_millis(33);

pub fn init(cx: &mut App) {
    workspace::register_path_open_interceptor(cx, intercept_audio_open);
}

/// Whether `path` is a file this tab opens (extension only, case-insensitive).
pub fn claims(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| AUDIO_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// `workspace::PathOpenInterceptor` for audio files: claim the path and
/// open (or focus) its tab. Declines for anything else and for a path
/// outside the primary worktree.
fn intercept_audio_open(
    workspace: &mut Workspace,
    path: &ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    if !claims(path.path.as_std_path()) {
        return false;
    }
    let Some(rel) = ggo_common::rel_in_primary_worktree(workspace, path, cx) else {
        return false;
    };
    open_audio_item(workspace, rel, window, cx);
    true
}

/// Open (or focus) the tab for worktree-relative `rel` -- one per file.
pub fn open_audio_item(
    workspace: &mut Workspace,
    rel: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let existing = workspace
        .items_of_type::<AudioItem>(cx)
        .find(|item| item.read(cx).rel() == rel);
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }
    let weak = workspace.weak_handle();
    let item = cx.new(|cx| AudioItem::new(rel, weak, window, cx));
    workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
}

/// The Import target for source `rel`: the same name under `assets/`
/// with the baked extension. A source already under `assets/` keeps its
/// directory (so `assets/sfx/jump.wav` → `assets/sfx/jump.adp`); anything
/// else lands flat in `assets/` (emerald's stem is the path under the
/// asset root, so this is what a world's `Sfx{stem}` will name).
pub fn default_import_target(rel: &str) -> String {
    let path = Path::new(rel);
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_string());
    if rel.starts_with("assets/") {
        let dir = path.parent().unwrap_or(Path::new(""));
        let dir = dir.to_string_lossy();
        if dir.is_empty() {
            format!("{name}.adp")
        } else {
            format!("{dir}/{name}.adp")
        }
    } else {
        format!("assets/{name}.adp")
    }
}

// ------------------------------------------------------------- view state

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Source,
    Baked,
}

pub(crate) enum ViewerState {
    Empty,
    Loading(String),
    Error { rel: String, message: String },
    Ready(Open),
}

pub(crate) struct Open {
    pub(crate) rel: String,
    pub(crate) is_adp: bool,
    pub(crate) decoded: Arc<Decoded>,
    waveform: Arc<Vec<(i16, i16)>>,
    /// The rate the bake (and Import) uses. For a `.adp` this is the
    /// file's own rate and cannot change.
    pub(crate) rate: u32,
    /// The `.adp` blob at `rate`, once the bake has landed.
    pub(crate) baked: Option<Arc<Vec<u8>>>,
    baking: bool,
    mode: Mode,
    looping: bool,
    /// Bake / import / playback problem, shown under the transport.
    pub(crate) error: Option<String>,
}

pub struct AudioPanel {
    focus_handle: FocusHandle,
    workspace: Option<WeakEntity<Workspace>>,
    /// Test hook: bypass workspace worktree discovery.
    pub(crate) root_override: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) state: ViewerState,
    load_generation: u64,
    bake_generation: u64,
    _load_task: Option<Task<()>>,
    _bake_task: Option<Task<()>>,
    /// Shared with the preview thread; the readout line shows its label.
    status: AudioStatus,
    preview: Option<Preview>,
    _playhead_task: Option<Task<()>>,
    /// The Import target path, editable.
    import_target: Entity<Editor>,
}

impl AudioPanel {
    pub fn new(
        workspace: Option<WeakEntity<Workspace>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            root_override: None,
            project_root: None,
            state: ViewerState::Empty,
            load_generation: 0,
            bake_generation: 0,
            _load_task: None,
            _bake_task: None,
            status: AudioStatus::new(),
            preview: None,
            _playhead_task: None,
            import_target: cx.new(|cx| Editor::single_line(window, cx)),
        }
    }

    fn refresh_root(&mut self, cx: &mut Context<Self>) {
        self.project_root = self.root_override.clone().or_else(|| {
            let workspace = self.workspace.as_ref()?.upgrade()?;
            let project = workspace.read(cx).project().clone();
            let worktree = project.read(cx).visible_worktrees(cx).next()?;
            Some(worktree.read(cx).abs_path().to_path_buf())
        });
        cx.notify();
    }

    /// Load worktree-relative `rel`. Deferred onto a task: the item is
    /// constructed from inside the workspace's own update, and
    /// `refresh_root` reads the workspace back.
    pub fn open_rel_path(&mut self, rel: &str, _window: &mut Window, cx: &mut Context<Self>) {
        if let ViewerState::Ready(open) = &self.state
            && open.rel == rel
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

    pub(crate) fn load_rel_path(&mut self, rel: &str, cx: &mut Context<Self>) {
        self.stop_preview(cx);
        self.load_generation += 1;
        let generation = self.load_generation;
        let rel = rel.to_string();
        let Some(root) = self.project_root.clone() else {
            self.state = ViewerState::Error {
                rel,
                message: "no project folder is open".to_string(),
            };
            cx.notify();
            return;
        };
        self.state = ViewerState::Loading(rel.clone());
        cx.notify();
        let path = root.join(&rel);
        let loaded = cx.background_spawn(async move { load::load(&path) });
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = loaded.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                match loaded {
                    Ok(loaded) => this.set_loaded(rel, loaded, cx),
                    Err(e) => {
                        this.state = ViewerState::Error {
                            rel,
                            message: format!("{e:#}"),
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_loaded(&mut self, rel: String, loaded: Loaded, cx: &mut Context<Self>) {
        let is_adp = loaded.adp.is_some();
        let rate = match &loaded.adp {
            Some(_) => loaded.decoded.rate_hz,
            None => ggo_audio::default_rate(Path::new(&rel)),
        };
        let target = default_import_target(&rel);
        self.state = ViewerState::Ready(Open {
            rel,
            is_adp,
            decoded: loaded.decoded,
            waveform: loaded.waveform,
            rate,
            baked: loaded.adp,
            baking: false,
            // A `.adp` has only one form; a source opens on the form the
            // hardware will play, since that is the question the tab
            // exists to answer.
            mode: Mode::Baked,
            looping: false,
            error: None,
        });
        if !is_adp {
            self.start_bake(cx);
        }
        // Through the buffer rather than `Editor::set_text`: this runs from
        // the load task, which has no window, and the target is plain
        // text with no selection to preserve.
        let buffer = self.import_target.read(cx).buffer().read(cx).as_singleton();
        if let Some(buffer) = buffer {
            buffer.update(cx, |buffer, cx| {
                buffer.set_text(target, cx);
            });
        }
    }

    fn open_mut(&mut self) -> Option<&mut Open> {
        match &mut self.state {
            ViewerState::Ready(open) => Some(open),
            _ => None,
        }
    }

    /// Re-bake the open source at its current rate, off-thread; a later
    /// bake (rate change, reload) invalidates this one by generation.
    fn start_bake(&mut self, cx: &mut Context<Self>) {
        self.bake_generation += 1;
        let generation = self.bake_generation;
        let Some(open) = self.open_mut() else {
            return;
        };
        if open.is_adp {
            return;
        }
        open.baking = true;
        open.baked = None;
        let decoded = open.decoded.clone();
        let rate = open.rate;
        cx.notify();
        let bake = cx.background_spawn(async move { Arc::new(ggo_audio::bake(&decoded, rate)) });
        self._bake_task = Some(cx.spawn(async move |this, cx| {
            let blob = bake.await;
            this.update(cx, |this, cx| {
                if this.bake_generation != generation {
                    return;
                }
                if let Some(open) = this.open_mut() {
                    open.baked = Some(blob);
                    open.baking = false;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(crate) fn set_rate(&mut self, rate: u32, cx: &mut Context<Self>) {
        let Some(open) = self.open_mut() else {
            return;
        };
        if open.is_adp || open.rate == rate {
            return;
        }
        open.rate = rate;
        self.stop_preview(cx);
        self.start_bake(cx);
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        let was_playing = self.preview.is_some();
        let Some(open) = self.open_mut() else {
            return;
        };
        if open.mode == mode {
            return;
        }
        open.mode = mode;
        self.stop_preview(cx);
        if was_playing {
            self.play(cx);
        }
        cx.notify();
    }

    fn toggle_loop(&mut self, cx: &mut Context<Self>) {
        let was_playing = self.preview.is_some();
        let Some(open) = self.open_mut() else {
            return;
        };
        open.looping = !open.looping;
        self.stop_preview(cx);
        if was_playing {
            self.play(cx);
        }
        cx.notify();
    }

    fn play_stop(&mut self, cx: &mut Context<Self>) {
        if self.preview.is_some() {
            self.stop_preview(cx);
        } else {
            self.play(cx);
        }
    }

    fn play(&mut self, cx: &mut Context<Self>) {
        let Some(open) = self.open_mut() else {
            return;
        };
        let spec = match open.mode {
            Mode::Source => Spec::Source(open.decoded.clone()),
            Mode::Baked => match &open.baked {
                Some(blob) => Spec::Baked(blob.clone()),
                None => {
                    open.error = Some("still baking — try again in a moment".to_string());
                    cx.notify();
                    return;
                }
            },
        };
        open.error = None;
        let looping = open.looping;
        self.status.reset_for_run();
        self.preview = Some(Preview::start(spec, looping, self.status.clone()));
        // Redraw the playhead until the thread reports done.
        self._playhead_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(PLAYHEAD_TICK).await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        let done = this.preview.as_ref().is_none_or(|p| p.is_done());
                        if done {
                            this.preview = None;
                        }
                        cx.notify();
                        !done
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn stop_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(preview) = self.preview.take() {
            preview.stop();
        }
        self._playhead_task = None;
        cx.notify();
    }

    // --------------------------------------------------------------- import

    pub(crate) fn import_target(&self, cx: &App) -> String {
        self.import_target.read(cx).text(cx).trim().to_string()
    }

    /// Whether Import would replace an existing file.
    pub(crate) fn import_would_overwrite(&self, cx: &App) -> bool {
        let target = self.import_target(cx);
        self.project_root
            .as_ref()
            .is_some_and(|root| !target.is_empty() && root.join(&target).exists())
    }

    /// Write the baked blob to the import target. The confirm (when the
    /// target exists) is the caller's; this is the half tests exercise.
    pub(crate) fn write_import(&mut self, cx: &mut Context<Self>) -> anyhow::Result<String> {
        let target = self.import_target(cx);
        if target.is_empty() {
            anyhow::bail!("import target is empty");
        }
        if !target.to_ascii_lowercase().ends_with(".adp") {
            anyhow::bail!("import target must end in .adp");
        }
        let root = self
            .project_root
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no project folder is open"))?;
        let blob = match &self.state {
            ViewerState::Ready(open) if open.is_adp => anyhow::bail!("already a .adp"),
            ViewerState::Ready(Open {
                baked: Some(blob), ..
            }) => blob.clone(),
            ViewerState::Ready(_) => anyhow::bail!("still baking — try again in a moment"),
            _ => anyhow::bail!("nothing is open"),
        };
        ggo_audio::write_adp(&root, &target, &blob)?;
        Ok(target)
    }

    fn import_impl(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let target = self.import_target(cx);
        let confirm = if self.import_would_overwrite(cx) {
            ggo_common::confirm_destructive(
                &format!("Overwrite {target}?"),
                "Overwrite",
                false,
                window,
                cx,
            )
        } else {
            Task::ready(true)
        };
        cx.spawn_in(window, async move |this, cx| {
            if !confirm.await {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                match this.write_import(cx) {
                    Ok(rel) => {
                        if let Some(open) = this.open_mut() {
                            open.error = None;
                        }
                        if let Some(workspace) = this.workspace.as_ref().and_then(|w| w.upgrade()) {
                            workspace.update(cx, |workspace, cx| {
                                open_audio_item(workspace, rel, window, cx)
                            });
                        }
                    }
                    Err(e) => {
                        if let Some(open) = this.open_mut() {
                            open.error = Some(format!("import failed: {e:#}"));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --------------------------------------------------- test-support hooks

    /// A file is decoded and the transport is on screen. `test-support`
    /// only, for `ggo_smoke`'s audio journey -- `state` is crate-private,
    /// so the tab is the only way in from another crate. Read-only.
    #[cfg(feature = "test-support")]
    pub fn test_is_ready(&self) -> bool {
        matches!(self.state, ViewerState::Ready(_))
    }

    /// The bake behind the Baked-mode transport has landed. `PlayStop`
    /// before this point is the documented "still baking" no-op rather
    /// than a preview, so a journey has to wait for it. `test-support`
    /// only.
    #[cfg(feature = "test-support")]
    pub fn test_is_baked(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if open.baked.is_some())
    }

    /// A preview run is live -- exactly the `self.preview.is_some()` the
    /// transport button reads to decide between Play and Stop. Says
    /// nothing about whether the preview THREAD has reached the end of
    /// the clip: that only becomes visible to the panel when the playhead
    /// task ticks. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_is_playing(&self) -> bool {
        self.preview.is_some()
    }

    /// The loop flag the next (or current) preview runs with.
    /// `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_is_looping(&self) -> bool {
        matches!(&self.state, ViewerState::Ready(open) if open.looping)
    }

    /// The bake / import / playback problem shown under the transport, or
    /// the load error for a file that never opened. `test-support` only.
    #[cfg(feature = "test-support")]
    pub fn test_error(&self) -> Option<String> {
        match &self.state {
            ViewerState::Error { message, .. } => Some(message.clone()),
            ViewerState::Ready(open) => open.error.clone(),
            _ => None,
        }
    }

    // --------------------------------------------------------------- render

    fn render_message(&self, message: String, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .justify_center()
            .items_center()
            .child(Label::new(message).color(Color::Muted))
            .bg(cx.theme().colors().panel_background)
            .into_any_element()
    }

    fn render_ready(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let ViewerState::Ready(open) = &self.state else {
            unreachable!("render_ready is only called in the Ready state");
        };
        let playing = self.preview.is_some();
        let progress = self.preview.as_ref().map(|p| p.progress());
        let secs = open.decoded.duration_ms() as f32 / 1000.0;
        let header = format!(
            "{} Hz · {} ch · {secs:.2} s{}",
            open.decoded.rate_hz,
            open.decoded.source_channels,
            if open.is_adp { " · baked" } else { "" }
        );

        let readout = match (&open.baked, open.baking) {
            (Some(blob), _) => {
                let bytes = ggo_audio::adp_region_bytes(blob).unwrap_or(0);
                let blocks = bytes / 64;
                let pct = bytes as u64 * 100 / ggo_audio::SAMPLE_REGION_BYTES as u64;
                let baked_secs = blocks as f32 * 120.0 / open.rate.max(1) as f32;
                format!(
                    "baked {} Hz · {blocks} blocks · {bytes} B · {pct}% of {} KiB · {baked_secs:.2} s",
                    open.rate,
                    ggo_audio::SAMPLE_REGION_BYTES / 1024
                )
            }
            (None, true) => format!("baking at {} Hz…", open.rate),
            (None, false) => String::new(),
        };
        let audio_label = self.status.state().label(playing);

        let weak = cx.weak_entity();
        let rate_menu = ContextMenu::build(window, cx, |mut menu, _window, _cx| {
            for rate in ggo_audio::RATES {
                let weak = weak.clone();
                menu = menu.entry(
                    SharedString::from(format!("{} kHz", rate / 1000)),
                    None,
                    move |_window, cx| {
                        weak.update(cx, |this, cx| this.set_rate(rate, cx)).ok();
                    },
                );
            }
            menu
        });
        let loop_weak = cx.weak_entity();
        let looping = open.looping;
        let mode = open.mode;
        let is_adp = open.is_adp;
        let can_import = !is_adp && open.baked.is_some();

        let transport = h_flex()
            .gap_2()
            .p_1()
            .items_center()
            .child(
                IconButton::new(
                    "ggo-audio-play",
                    if playing {
                        IconName::Stop
                    } else {
                        IconName::PlayFilled
                    },
                )
                .icon_size(IconSize::Small)
                .tooltip(ui::Tooltip::text(if playing {
                    "Stop (space)"
                } else {
                    "Play (space)"
                }))
                .on_click(cx.listener(|this, _, _, cx| this.play_stop(cx))),
            )
            .child(
                Checkbox::new("ggo-audio-loop", ToggleState::from(looping))
                    .label("Loop")
                    .on_click(move |_toggle, _window, cx| {
                        loop_weak.update(cx, |this, cx| this.toggle_loop(cx)).ok();
                    }),
            )
            .child(
                Button::new("ggo-audio-mode-source", "Source")
                    .toggle_state(mode == Mode::Source)
                    .disabled(is_adp)
                    .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::Source, cx))),
            )
            .child(
                Button::new("ggo-audio-mode-baked", "Baked")
                    .toggle_state(mode == Mode::Baked)
                    .on_click(cx.listener(|this, _, _, cx| this.set_mode(Mode::Baked, cx))),
            )
            .child(if is_adp {
                Label::new(format!("{} Hz", open.rate))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .into_any_element()
            } else {
                DropdownMenu::new(
                    "ggo-audio-rate",
                    format!("{} kHz", open.rate / 1000),
                    rate_menu,
                )
                .into_any_element()
            })
            .child(div().flex_1())
            .when(!is_adp, |this| {
                this.child(div().w(px(280.)).child(self.import_target.clone()))
                    .child(
                        Button::new("ggo-audio-import", "Import as .adp")
                            .disabled(!can_import)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.import_impl(window, cx)),
                            ),
                    )
            });

        v_flex()
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .gap_2()
                    .p_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(Label::new(open.rel.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(header)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(self.render_waveform(open.waveform.clone(), progress, cx))
            .child(transport)
            .child(
                h_flex()
                    .gap_2()
                    .px_1()
                    .child(
                        Label::new(readout)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .children(
                        audio_label.map(|label| {
                            Label::new(label).size(LabelSize::Small).color(Color::Muted)
                        }),
                    ),
            )
            .children(open.error.as_ref().map(|e| {
                div().px_1().child(
                    ggo_common::CopyableText::new("ggo-audio-error-copy", e.clone())
                        .size(LabelSize::Small),
                )
            }))
            .into_any_element()
    }

    fn render_waveform(
        &self,
        waveform: Arc<Vec<(i16, i16)>>,
        progress: Option<f32>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let background = colors.editor_background;
        let wave = colors.text_accent;
        let midline = colors.border;
        let playhead = colors.border_focused;
        gpui::canvas(
            |_, _, _| {},
            move |bounds: Bounds<Pixels>, (), window, _cx| {
                window.paint_quad(gpui::fill(bounds, background));
                let width: f32 = bounds.size.width.into();
                let height: f32 = bounds.size.height.into();
                let columns = width.max(1.0) as usize;
                let mid_y = bounds.origin.y + px(height / 2.0);
                window.paint_quad(gpui::fill(
                    Bounds::new(
                        point(bounds.origin.x, mid_y),
                        size(bounds.size.width, px(1.)),
                    ),
                    midline,
                ));
                if !waveform.is_empty() {
                    let half = height / 2.0;
                    for x in 0..columns {
                        let i = x * waveform.len() / columns;
                        let (lo, hi) = waveform[i.min(waveform.len() - 1)];
                        let top = mid_y - px(hi as f32 / 32768.0 * half);
                        let bottom = mid_y - px(lo as f32 / 32768.0 * half);
                        let h = (bottom - top).max(px(1.));
                        window.paint_quad(gpui::fill(
                            Bounds::new(
                                point(bounds.origin.x + px(x as f32), top),
                                size(px(1.), h),
                            ),
                            wave,
                        ));
                    }
                }
                if let Some(progress) = progress {
                    let x = bounds.origin.x + px(width * progress.clamp(0.0, 1.0));
                    window.paint_quad(gpui::fill(
                        Bounds::new(point(x, bounds.origin.y), size(px(2.), bounds.size.height)),
                        playhead,
                    ));
                }
            },
        )
        .w_full()
        .h(px(WAVEFORM_HEIGHT_PX))
        .into_any_element()
    }
}

impl Focusable for AudioPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AudioPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            ViewerState::Empty => self.render_message(
                "Open a .wav, .ogg or .adp from the project panel".to_string(),
                cx,
            ),
            ViewerState::Loading(rel) => self.render_message(format!("decoding {rel}…"), cx),
            ViewerState::Error { rel, message } => {
                self.render_message(format!("{rel}: {message}"), cx)
            }
            ViewerState::Ready(_) => self.render_ready(window, cx),
        };
        div()
            .id("ggo-audio-panel")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .on_action(cx.listener(|this, _: &PlayStop, _window, cx| this.play_stop(cx)))
            .on_action(cx.listener(|this, _: &ToggleLoop, _window, cx| this.toggle_loop(cx)))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project, WorktreeId};
    use workspace::item::Item;
    use workspace::{AppState, MultiWorkspace};

    #[gpui::test]
    fn init_registers_without_panic(cx: &mut gpui::App) {
        init(cx);
    }

    #[test]
    fn claims_exactly_the_audio_extensions() {
        assert!(claims(Path::new("sfx/jump.wav")));
        assert!(claims(Path::new("music/theme.OGG")));
        assert!(claims(Path::new("assets/theme.adp")));
        assert!(!claims(Path::new("assets/hero.png")));
        assert!(!claims(Path::new("notes")));
    }

    #[test]
    fn the_import_target_lands_under_assets_with_the_baked_extension() {
        assert_eq!(
            default_import_target("audio-src/jump.wav"),
            "assets/jump.adp"
        );
        assert_eq!(
            default_import_target("assets/sfx/jump.wav"),
            "assets/sfx/jump.adp"
        );
        assert_eq!(
            default_import_target("assets/theme.ogg"),
            "assets/theme.adp"
        );
        assert_eq!(default_import_target("theme.ogg"), "assets/theme.adp");
    }

    /// A one-second 16 kHz mono PCM16 triangle wave, written as a real
    /// RIFF file under `root/rel`.
    fn write_wav(root: &Path, rel: &str, rate: u32, seconds: u32) {
        let n = (rate * seconds) as usize;
        let period = 40usize;
        let samples: Vec<i16> = (0..n)
            .map(|i| {
                let p = i % period;
                let half = period / 2;
                let v = if p < half {
                    (p as i32 * 16_000 / half as i32) - 8000
                } else {
                    8000 - ((p - half) as i32 * 16_000 / half as i32)
                };
                v as i16
            })
            .collect();
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, out).unwrap();
    }

    async fn ready_item<'a>(
        cx: &'a mut TestAppContext,
        root: &Path,
        rel: &str,
    ) -> (Entity<AudioItem>, &'a mut gpui::VisualTestContext) {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init(cx);
        });
        let root = root.to_path_buf();
        let rel = rel.to_string();
        let (item, cx) =
            cx.add_window_view(|window, cx| AudioItem::new_for_test(rel, root, window, cx));
        cx.run_until_parked();
        (item, cx)
    }

    fn ready(panel: &AudioPanel) -> &Open {
        match &panel.state {
            ViewerState::Ready(open) => open,
            ViewerState::Error { rel, message } => {
                panic!("expected Ready, got error {rel}: {message}")
            }
            _ => panic!("expected Ready"),
        }
    }

    /// Opening a source file decodes it, bakes it at emerald's default
    /// rate for the container, and prefills the import target; changing
    /// the rate re-bakes with the new rate in the header.
    #[gpui::test]
    async fn test_a_wav_opens_baked_at_the_default_rate_and_rebakes_on_rate_change(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 32_000, 1);
        let (item, cx) = ready_item(cx, dir.path(), "audio-src/jump.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());

        panel.read_with(cx, |panel, cx| {
            let open = ready(panel);
            assert_eq!(open.decoded.rate_hz, 32_000);
            assert_eq!(open.decoded.samples.len(), 32_000);
            assert_eq!(open.rate, 16_000, "wav defaults to the SFX rate");
            assert!(!open.is_adp);
            let blob = open.baked.as_ref().expect("bake landed");
            let (header, _) = ggo_asset_formats::parse_adp(blob).unwrap();
            assert_eq!(header.rate_hz, 16_000);
            assert_eq!(header.block_count, 16_000 / 120 + 1);
            assert_eq!(panel.import_target(cx), "assets/jump.adp");
            assert_eq!(item.read(cx).tab_content_text(0, cx).as_ref(), "jump.wav");
        });

        panel.update(cx, |panel, cx| panel.set_rate(8_000, cx));
        cx.run_until_parked();
        panel.read_with(cx, |panel, _| {
            let open = ready(panel);
            let (header, _) = ggo_asset_formats::parse_adp(open.baked.as_ref().unwrap()).unwrap();
            assert_eq!(header.rate_hz, 8_000);
            assert_eq!(header.block_count, 8_000 / 120 + 1);
        });
    }

    /// Import writes the baked blob to the target, the target then counts
    /// as an overwrite, and the written file opens as a read-only `.adp`
    /// tab whose rate is the header's.
    #[gpui::test]
    async fn test_import_writes_the_adp_and_it_reopens_as_baked(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        write_wav(dir.path(), "audio-src/jump.wav", 16_000, 1);
        let (item, cx) = ready_item(cx, dir.path(), "audio-src/jump.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());

        panel.update(cx, |panel, cx| {
            assert!(!panel.import_would_overwrite(cx));
            let rel = panel.write_import(cx).expect("import writes");
            assert_eq!(rel, "assets/jump.adp");
            assert!(panel.import_would_overwrite(cx), "the target now exists");
        });
        let written = std::fs::read(dir.path().join("assets/jump.adp")).unwrap();
        panel.read_with(cx, |panel, _| {
            assert_eq!(&written, ready(panel).baked.as_ref().unwrap().as_ref());
        });

        let (adp_item, cx) = cx.add_window_view(|window, cx| {
            AudioItem::new_for_test(
                "assets/jump.adp".into(),
                dir.path().to_path_buf(),
                window,
                cx,
            )
        });
        cx.run_until_parked();
        let adp_panel = adp_item.read_with(cx, |item, _| item.panel().clone());
        adp_panel.update(cx, |panel, cx| {
            let open = ready(panel);
            assert!(open.is_adp);
            assert_eq!(open.rate, 16_000);
            assert!(open.baked.is_some(), "the file is its own bake");
            assert_eq!(open.decoded.samples.len(), (16_000 / 120 + 1) * 120);
            let err = panel.write_import(cx).unwrap_err();
            assert!(err.to_string().contains("already a .adp"), "{err}");
        });
    }

    #[gpui::test]
    async fn test_a_missing_file_is_an_error_state_not_a_panic(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (item, cx) = ready_item(cx, dir.path(), "audio-src/nope.wav").await;
        let panel = item.read_with(cx, |item, _| item.panel().clone());
        panel.read_with(cx, |panel, _| match &panel.state {
            ViewerState::Error { rel, message } => {
                assert_eq!(rel, "audio-src/nope.wav");
                assert!(message.contains("nope.wav"), "{message}");
            }
            _ => panic!("expected Error state"),
        });
    }

    async fn routed_project(cx: &mut TestAppContext) -> Entity<Project> {
        cx.update(|cx| {
            AppState::test(cx);
            editor::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/proj",
            serde_json::json!({ "sfx": { "jump.wav": "", "theme.ogg": "" }, "notes.txt": "" }),
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

    /// The registered predicate claims audio files (so the project panel
    /// opens no text buffer for them) and adds ONE tab per file; a re-click
    /// activates instead of duplicating; anything else is declined.
    #[gpui::test]
    async fn test_audio_click_opens_one_center_tab_per_file(cx: &mut TestAppContext) {
        let project = routed_project(cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let worktree_id = worktree_id(&project, cx);

        for rel in ["sfx/jump.wav", "sfx/jump.wav", "sfx/theme.ogg"] {
            let claimed = workspace.update_in(cx, |workspace, window, cx| {
                workspace.intercept_path_open(&project_path(worktree_id, rel), window, cx)
            });
            assert!(claimed, "{rel} must be claimed");
            cx.run_until_parked();
        }
        let mut items: Vec<_> = workspace.read_with(cx, |workspace, cx| {
            workspace
                .items_of_type::<AudioItem>(cx)
                .map(|item| item.read(cx).rel().to_string())
                .collect()
        });
        items.sort();
        assert_eq!(
            items,
            vec!["sfx/jump.wav", "sfx/theme.ogg"],
            "one tab per file"
        );

        let claimed = workspace.update_in(cx, |workspace, window, cx| {
            workspace.intercept_path_open(&project_path(worktree_id, "notes.txt"), window, cx)
        });
        assert!(!claimed, "everything else opens the normal way");
    }
}
