//! The `ggo: select emulator version` card: lists the emulator modules the
//! runtime can switch to and rewrites `~/.ggo/emulator.json` through
//! `EmuRuntime::select`. The runtime's config watcher does the hot swap.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use fuzzy::{StringMatch, StringMatchCandidate};
use ggo_common::picker_card::{self, PickerCard};
use ggo_emu_wasm::sources::{EmulatorVersion, ForgejoConfig};
use ggo_emu_wasm::{EmuRuntime, EmulatorConfig, SourceConfig};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    PathPromptOptions, Render, Task, WeakEntity, Window,
};
use picker::{Picker, PickerDelegate};
use ui::prelude::*;
use ui::{ListItem, ListItemSpacing};
use util::ResultExt as _;
use workspace::{ModalView, OpenOptions, Workspace};

#[derive(Clone, Debug, PartialEq, Eq)]
enum RowAction {
    Select(SourceConfig),
    ChooseLocalFile,
    EditConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    label: String,
    current: bool,
    action: RowAction,
}

impl Row {
    fn select(label: impl Into<String>, current: bool, source: SourceConfig) -> Self {
        Row {
            label: label.into(),
            current,
            action: RowAction::Select(source),
        }
    }
}

enum Listing {
    NotApplicable,
    Loading,
    Loaded,
    Failed(String),
}

/// The tag a Forgejo version was listed under; its id is
/// `{base_url}/{owner}/{repo}@{tag}`.
fn version_tag(version: &EmulatorVersion) -> Option<&str> {
    version.id.rsplit_once('@').map(|(_, tag)| tag)
}

fn build_rows(config: &SourceConfig, versions: &[EmulatorVersion]) -> Vec<Row> {
    let mut rows = vec![
        Row::select(
            "Bundled",
            *config == SourceConfig::Bundled,
            SourceConfig::Bundled,
        ),
        Row {
            label: "Local file…".into(),
            current: false,
            action: RowAction::ChooseLocalFile,
        },
    ];
    match config {
        SourceConfig::Path(path) => rows.push(Row::select(
            path.display().to_string(),
            true,
            config.clone(),
        )),
        SourceConfig::Forgejo(forgejo) => {
            let pinned = |tag: &str| {
                SourceConfig::Forgejo(ForgejoConfig {
                    tag: tag.to_string(),
                    ..forgejo.clone()
                })
            };
            rows.push(Row::select(
                "Forgejo: latest",
                forgejo.tag == "latest",
                pinned("latest"),
            ));
            for version in versions {
                let Some(tag) = version_tag(version) else {
                    continue;
                };
                rows.push(Row::select(
                    version.label.clone(),
                    forgejo.tag == tag,
                    pinned(tag),
                ));
            }
        }
        SourceConfig::Url(url) => rows.push(Row::select(url.clone(), true, config.clone())),
        SourceConfig::Bundled => {}
    }
    rows.push(Row {
        label: "Edit ~/.ggo/emulator.json".into(),
        current: false,
        action: RowAction::EditConfig,
    });
    rows
}

pub struct EmulatorPicker {
    runtime: Entity<EmuRuntime>,
    workspace: WeakEntity<Workspace>,
    picker: Entity<Picker<EmulatorPickerDelegate>>,
    listing: Listing,
    error: Option<String>,
    listing_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    focus_handle: FocusHandle,
}

impl EmulatorPicker {
    pub fn new(
        runtime: Entity<EmuRuntime>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = runtime.read(cx).config().source.clone();
        let is_forgejo = matches!(config, SourceConfig::Forgejo(_));
        let delegate = EmulatorPickerDelegate {
            modal: cx.weak_entity(),
            config,
            rows: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .embedded()
                .initial_width(picker_card::PICKER_WIDTH)
                .max_height(picker_card::PICKER_MAX_HEIGHT)
        });
        let mut this = Self {
            runtime,
            workspace,
            picker,
            listing: Listing::NotApplicable,
            error: None,
            listing_task: None,
            action_task: None,
            focus_handle: cx.focus_handle(),
        };
        this.set_versions(&[], window, cx);
        if is_forgejo {
            this.load_versions(window, cx);
        }
        this
    }

    fn set_versions(
        &mut self,
        versions: &[EmulatorVersion],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.picker.update(cx, |picker, cx| {
            picker.delegate.rows = build_rows(&picker.delegate.config, versions);
            picker.refresh(window, cx);
        });
    }

    fn load_versions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.listing = Listing::Loading;
        let versions = self.runtime.read(cx).source().list_versions();
        self.listing_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = versions.await;
            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(versions) => {
                        this.listing = Listing::Loaded;
                        this.set_versions(&versions, window, cx);
                    }
                    Err(error) => this.listing = Listing::Failed(format!("{error:#}")),
                }
                cx.notify();
            })
            .log_err();
        }));
    }

    fn confirm_highlighted(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(action) = self.picker.read(cx).delegate.selected_action() {
            self.confirm(action, window, cx);
        }
    }

    fn confirm(&mut self, action: RowAction, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        cx.notify();
        match action {
            RowAction::Select(source) => {
                let task = self.select(source, cx);
                self.finish_with(task, cx);
            }
            RowAction::ChooseLocalFile => self.choose_local_file(cx),
            RowAction::EditConfig => self.edit_config(window, cx),
        }
    }

    fn select(&self, source: SourceConfig, cx: &mut Context<Self>) -> Task<Result<bool>> {
        let selected = self.runtime.update(cx, |runtime, cx| {
            runtime.select(EmulatorConfig { source }, cx)
        });
        cx.background_spawn(async move {
            selected.await?;
            Ok(true)
        })
    }

    fn finish_with(&mut self, task: Task<Result<bool>>, cx: &mut Context<Self>) {
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(true) => cx.emit(DismissEvent),
                Ok(false) => {}
                Err(error) => {
                    this.error = Some(format!("{error:#}"));
                    cx.notify();
                }
            })
            .log_err();
        }));
    }

    fn choose_local_file(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select an emulator module".into()),
        });
        let task = cx.spawn(async move |this, cx| {
            let picked = paths.await.context("file prompt closed")??;
            let Some(path) = picked.and_then(|paths| paths.into_iter().next()) else {
                return Ok(false);
            };
            this.update(cx, |this, cx| this.select(SourceConfig::Path(path), cx))?
                .await
        });
        self.finish_with(task, cx);
    }

    fn edit_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let fs = workspace.read(cx).project().read(cx).fs().clone();
        let runtime = self.runtime.read(cx);
        let config = runtime.config().clone();
        let path = runtime.config_path().to_path_buf();
        let task = cx.spawn_in(window, async move |_, cx| {
            if !fs.is_file(&path).await {
                if let Some(parent) = path.parent() {
                    fs.create_dir(parent).await?;
                }
                fs.write(&path, &serde_json::to_vec_pretty(&config)?)
                    .await
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            workspace
                .update_in(cx, |workspace, window, cx| {
                    workspace.open_abs_path(path, OpenOptions::default(), window, cx)
                })?
                .await?;
            Ok(true)
        });
        self.finish_with(task, cx);
    }
}

impl EventEmitter<DismissEvent> for EmulatorPicker {}

impl Focusable for EmulatorPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ModalView for EmulatorPicker {}

impl Render for EmulatorPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = match &self.listing {
            Listing::Loading => Some(picker_card::preview_placeholder("Loading releases…")),
            Listing::Failed(message) => Some(picker_card::preview_placeholder(message.clone())),
            Listing::NotApplicable | Listing::Loaded => None,
        };
        let description = self.runtime.read(cx).source().describe();
        PickerCard::new("ggo-emu-version", "Select emulator version", description)
            .track_focus(&self.focus_handle)
            .body(
                v_flex()
                    .gap_1()
                    .child(self.picker.clone())
                    .children(status)
                    .children(self.error.clone().map(|error| {
                        ggo_common::CopyableText::new("ggo-emu-version-error-copy", error)
                            .size(LabelSize::Small)
                    })),
            )
            .confirm("ggo-emu-version-select", "Select")
            .render(
                cx.listener(|this, _, window, cx| this.confirm_highlighted(window, cx)),
                cx,
            )
    }
}

struct EmulatorPickerDelegate {
    modal: WeakEntity<EmulatorPicker>,
    config: SourceConfig,
    rows: Vec<Row>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl EmulatorPickerDelegate {
    fn row_at(&self, ix: usize) -> Option<&Row> {
        let candidate = self.matches.get(ix)?.candidate_id;
        self.rows.get(candidate)
    }

    fn selected_action(&self) -> Option<RowAction> {
        self.row_at(self.selected_index)
            .map(|row| row.action.clone())
    }
}

impl PickerDelegate for EmulatorPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "emulator version"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Select an emulator module…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates: Vec<StringMatchCandidate> = self
            .rows
            .iter()
            .enumerate()
            .map(|(id, row)| StringMatchCandidate::new(id, &row.label))
            .collect();
        cx.spawn_in(window, async move |this, cx| {
            let matches = picker_card::matches_for(candidates, query.clone(), background).await;
            this.update(cx, |this, _| {
                this.delegate.matches = matches;
                this.delegate.selected_index = picker_card::reselect_index(
                    this.delegate.selected_index,
                    &query,
                    this.delegate.matches.len(),
                );
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(action) = self.selected_action() else {
            return;
        };
        self.modal
            .update(cx, |modal, cx| modal.confirm(action, window, cx))
            .log_err();
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let row = self.row_at(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(h_flex().gap_2().child(Label::new(row.label.clone())).when(
                    row.current,
                    |this| {
                        this.child(
                            Label::new("(current)")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    },
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SelectEmulatorVersion, init};
    use gpui::{Action as _, TestAppContext};
    use project::{FakeFs, Fs as _, Project};
    use workspace::{AppState, MultiWorkspace};

    fn forgejo_config(tag: &str) -> SourceConfig {
        SourceConfig::Forgejo(ForgejoConfig {
            base_url: "https://git.example".into(),
            owner: "gemdrop".into(),
            repo: "ggo".into(),
            asset: "ggo_emu.wasm".into(),
            tag: tag.into(),
            token: None,
        })
    }

    #[test]
    fn rows_mark_the_active_source() {
        let rows = build_rows(&SourceConfig::Url("https://host/a.wasm".into()), &[]);
        let labels: Vec<_> = rows
            .iter()
            .map(|row| (row.label.as_str(), row.current))
            .collect();
        assert_eq!(
            labels,
            [
                ("Bundled", false),
                ("Local file…", false),
                ("https://host/a.wasm", true),
                ("Edit ~/.ggo/emulator.json", false),
            ]
        );

        let rows = build_rows(&SourceConfig::Path("/work/ggo_emu.wasm".into()), &[]);
        assert_eq!(rows[2].label, "/work/ggo_emu.wasm");
        assert!(rows[2].current);
    }

    #[gpui::test]
    async fn test_picking_a_forgejo_release_rewrites_the_config(cx: &mut TestAppContext) {
        cx.update(|cx| {
            AppState::test(cx);
            // Registers `ui_input::ERASED_EDITOR_FACTORY`, which the picker's query field needs.
            editor::init(cx);
            init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.create_dir("/home/.ggo".as_ref()).await.expect("mkdir");
        fs.insert_file(
            "/home/.ggo/emulator.json",
            serde_json::to_vec(&EmulatorConfig {
                source: forgejo_config("latest"),
            })
            .expect("serialize"),
        )
        .await;
        let http = http_client::FakeHttpClient::create(|request| async move {
            let status = if request.uri().path() == "/api/v1/repos/gemdrop/ggo/releases" {
                200
            } else {
                404
            };
            Ok(http_client::Response::builder()
                .status(status)
                .body(ggo_emu_wasm::sources::RELEASES.as_bytes().to_vec().into())?)
        });
        cx.update(|cx| {
            EmuRuntime::init_with_config_path(
                fs.clone(),
                http,
                "/home/.ggo/emulator.json".into(),
                "/cache".into(),
                cx,
            );
        });
        let project = Project::test(fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(200));
        cx.executor().run_until_parked();

        workspace.update_in(cx, |_, window, cx| {
            window.dispatch_action(SelectEmulatorVersion.boxed_clone(), cx);
        });
        cx.executor().run_until_parked();
        let modal = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<EmulatorPicker>(cx)
            })
            .expect("the action opens the picker");
        let picker = modal.read_with(cx, |modal, _| modal.picker.clone());
        let labels: Vec<String> = picker.read_with(cx, |picker, _| {
            (0..picker.delegate.match_count())
                .filter_map(|ix| picker.delegate.row_at(ix).map(|row| row.label.clone()))
                .collect()
        });
        assert_eq!(
            labels,
            [
                "Bundled",
                "Local file…",
                "Forgejo: latest",
                "v0.3.0-rc1 (pre-release)",
                "v0.2.0",
                "Edit ~/.ggo/emulator.json",
            ]
        );

        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.set_selected_index(4, window, cx);
            picker.delegate.confirm(false, window, cx);
        });
        cx.executor().run_until_parked();

        let written = fs
            .load("/home/.ggo/emulator.json".as_ref())
            .await
            .expect("config");
        let written: EmulatorConfig = serde_json::from_str(&written).expect("valid config");
        assert_eq!(written.source, forgejo_config("v0.2.0"));
        assert!(
            workspace.read_with(cx, |workspace, cx| workspace
                .active_modal::<EmulatorPicker>(cx)
                .is_none()),
            "confirming dismisses the picker"
        );
    }
}
