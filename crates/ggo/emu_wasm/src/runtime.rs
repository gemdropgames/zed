//! The process-wide emulator module: which source it comes from
//! (`~/.ggo/emulator.json`), fetching and caching it, compiling it, and
//! swapping it when the config or a local module file changes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Global, SharedString, Task};
use http_client::HttpClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use util::ResultExt as _;

use crate::sources::{
    BundledSource, EmulatorSource, EmulatorVersion, ForgejoConfig, ForgejoSource, HttpSource,
    LocalSource,
};
use crate::{BUNDLED_WASM, LoadedEmulator};

const WATCH_LATENCY: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceConfig {
    #[default]
    Bundled,
    Path(PathBuf),
    Url(String),
    Forgejo(ForgejoConfig),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmulatorConfig {
    #[serde(default)]
    pub source: SourceConfig,
}

pub fn config_path() -> PathBuf {
    util::paths::home_dir().join(".ggo/emulator.json")
}

pub fn source_for(
    config: &SourceConfig,
    fs: Arc<dyn Fs>,
    http: Arc<dyn HttpClient>,
) -> Arc<dyn EmulatorSource> {
    match config {
        SourceConfig::Bundled => Arc::new(BundledSource),
        SourceConfig::Path(path) => Arc::new(LocalSource {
            path: path.clone(),
            fs,
        }),
        SourceConfig::Url(url) => Arc::new(HttpSource {
            url: url.clone(),
            http,
        }),
        SourceConfig::Forgejo(config) => Arc::new(ForgejoSource {
            config: config.clone(),
            http,
        }),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeStatus {
    Loading,
    Ready,
    Failed(SharedString),
}

pub struct EmulatorChanged;

struct GlobalEmuRuntime(Entity<EmuRuntime>);

impl Global for GlobalEmuRuntime {}

pub struct EmuRuntime {
    fs: Arc<dyn Fs>,
    http: Arc<dyn HttpClient>,
    config_path: PathBuf,
    cache_dir: PathBuf,
    current: Option<Arc<LoadedEmulator>>,
    status: RuntimeStatus,
    config: EmulatorConfig,
    config_is_malformed: bool,
    load_task: Option<Task<()>>,
    fallback_task: Option<Task<()>>,
    config_watch_task: Option<Task<()>>,
    module_watch_task: Option<Task<()>>,
}

impl EventEmitter<EmulatorChanged> for EmuRuntime {}

impl EmuRuntime {
    pub fn init(fs: Arc<dyn Fs>, http: Arc<dyn HttpClient>, cx: &mut App) {
        Self::init_with_config_path(
            fs,
            http,
            config_path(),
            paths::data_dir().join("ggo-emu"),
            cx,
        );
    }

    pub fn init_with_config_path(
        fs: Arc<dyn Fs>,
        http: Arc<dyn HttpClient>,
        config_path: PathBuf,
        cache_dir: PathBuf,
        cx: &mut App,
    ) {
        let runtime = cx.new(|cx| {
            let mut runtime = EmuRuntime {
                fs,
                http,
                config_path,
                cache_dir,
                current: None,
                status: RuntimeStatus::Loading,
                config: EmulatorConfig::default(),
                config_is_malformed: false,
                load_task: None,
                fallback_task: None,
                config_watch_task: None,
                module_watch_task: None,
            };
            runtime.watch_config(cx);
            runtime
        });
        cx.set_global(GlobalEmuRuntime(runtime));
    }

    pub fn global(cx: &App) -> Option<Entity<EmuRuntime>> {
        cx.try_global::<GlobalEmuRuntime>()
            .map(|global| global.0.clone())
    }

    pub fn current(&self) -> Option<Arc<LoadedEmulator>> {
        self.current.clone()
    }

    pub fn status(&self) -> RuntimeStatus {
        self.status.clone()
    }

    pub fn config(&self) -> &EmulatorConfig {
        &self.config
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn source(&self) -> Arc<dyn EmulatorSource> {
        source_for(&self.config.source, self.fs.clone(), self.http.clone())
    }

    /// Writes the config file; the config watcher picks the change up and
    /// reloads.
    pub fn select(&mut self, config: EmulatorConfig, cx: &mut Context<Self>) -> Task<Result<()>> {
        let fs = self.fs.clone();
        let config_path = self.config_path.clone();
        cx.background_spawn(async move {
            let json = serde_json::to_vec_pretty(&config)?;
            if let Some(parent) = config_path.parent() {
                fs.create_dir(parent).await?;
            }
            fs.write(&config_path, &json)
                .await
                .with_context(|| format!("writing {}", config_path.display()))
        })
    }

    fn watch_config(&mut self, cx: &mut Context<Self>) {
        let fs = self.fs.clone();
        let config_path = self.config_path.clone();
        self.config_watch_task = Some(cx.spawn(async move |this, cx| {
            let Some(parent) = config_path.parent() else {
                return;
            };
            let (mut events, _watcher) = fs.watch(parent, WATCH_LATENCY).await;
            let initial = read_config(fs.as_ref(), &config_path).await;
            if this
                .update(cx, |this, cx| this.apply_config(initial, cx))
                .is_err()
            {
                return;
            }
            while let Some(batch) = events.next().await {
                if !batch.iter().any(|event| event.path == config_path) {
                    continue;
                }
                let config = read_config(fs.as_ref(), &config_path).await;
                if this
                    .update(cx, |this, cx| this.apply_config(config, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
    }

    fn apply_config(&mut self, config: Result<EmulatorConfig, String>, cx: &mut Context<Self>) {
        match config {
            Ok(config) => {
                let changed = config != self.config || self.config_is_malformed;
                let first_load = self.load_task.is_none();
                self.config_is_malformed = false;
                if changed || first_load {
                    self.config = config;
                    self.watch_local_module(cx);
                    self.reload(cx);
                }
            }
            Err(message) => {
                self.config_is_malformed = true;
                self.fail(message, cx);
            }
        }
    }

    fn watch_local_module(&mut self, cx: &mut Context<Self>) {
        let SourceConfig::Path(module_path) = &self.config.source else {
            self.module_watch_task = None;
            return;
        };
        let module_path = module_path.clone();
        let fs = self.fs.clone();
        self.module_watch_task = Some(cx.spawn(async move |this, cx| {
            let Some(parent) = module_path.parent() else {
                return;
            };
            let (mut events, _watcher) = fs.watch(parent, WATCH_LATENCY).await;
            while let Some(batch) = events.next().await {
                if !batch.iter().any(|event| event.path == module_path) {
                    continue;
                }
                if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                    return;
                }
            }
        }));
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.status = RuntimeStatus::Loading;
        cx.notify();

        let source = self.source();
        let config = self.config.source.clone();
        let cache_dir = self.cache_dir.clone();
        let fs = self.fs.clone();
        let http = self.http.clone();
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(load_module(source, config, fs, http, cache_dir))
                .await;
            this.update(cx, |this, cx| match loaded {
                Ok(loaded) => {
                    this.current = Some(Arc::new(loaded));
                    this.status = RuntimeStatus::Ready;
                    cx.emit(EmulatorChanged);
                    cx.notify();
                }
                Err(error) => this.fail(format!("{error:#}"), cx),
            })
            .log_err();
        }));
    }

    fn fail(&mut self, message: String, cx: &mut Context<Self>) {
        log::error!("emulator module: {message}");
        self.status = RuntimeStatus::Failed(message.into());
        cx.notify();
        if self.current.is_some() {
            return;
        }
        self.fallback_task = Some(cx.spawn(async move |this, cx| {
            let compiled = cx
                .background_spawn(async { LoadedEmulator::compile(BUNDLED_WASM, "Bundled") })
                .await;
            this.update(cx, |this, cx| {
                if this.current.is_some() {
                    return;
                }
                if let Some(loaded) = compiled.log_err() {
                    this.current = Some(Arc::new(loaded));
                    cx.emit(EmulatorChanged);
                    cx.notify();
                }
            })
            .log_err();
        }));
    }
}

pub fn current_emulator(cx: &App) -> Result<Arc<LoadedEmulator>> {
    let runtime = EmuRuntime::global(cx).context("emulator runtime is not initialized")?;
    let runtime = runtime.read(cx);
    if let Some(current) = runtime.current() {
        return Ok(current);
    }
    match runtime.status() {
        RuntimeStatus::Failed(message) => Err(anyhow!("emulator module failed: {message}")),
        RuntimeStatus::Loading | RuntimeStatus::Ready => {
            Err(anyhow!("emulator module is still loading"))
        }
    }
}

/// A missing file is the default config; an unreadable or malformed one is an
/// error message for the status line.
async fn read_config(fs: &dyn Fs, path: &Path) -> Result<EmulatorConfig, String> {
    if !fs.is_file(path).await {
        return Ok(EmulatorConfig::default());
    }
    let text = fs
        .load(path)
        .await
        .map_err(|error| format!("~/.ggo/emulator.json: {error:#}"))?;
    serde_json::from_str(&text).map_err(|error| format!("~/.ggo/emulator.json: {error}"))
}

async fn load_module(
    source: Arc<dyn EmulatorSource>,
    config: SourceConfig,
    fs: Arc<dyn Fs>,
    http: Arc<dyn HttpClient>,
    cache_dir: PathBuf,
) -> Result<LoadedEmulator> {
    let versions = source.list_versions().await?;
    let (version, label) = match &config {
        SourceConfig::Forgejo(forgejo) => {
            let resolver = ForgejoSource {
                config: forgejo.clone(),
                http,
            };
            let version = resolver.resolve(&versions)?;
            let marker = format!("{}/{}@", forgejo.owner, forgejo.repo);
            let tag = version
                .id
                .split_once(&marker)
                .map_or(forgejo.tag.as_str(), |(_, tag)| tag);
            let label = format!("Forgejo: {}/{}@{tag}", forgejo.owner, forgejo.repo);
            (version, SharedString::from(label))
        }
        _ => {
            let version = versions
                .into_iter()
                .next()
                .context("the source lists no versions")?;
            (version, source.describe())
        }
    };

    let cacheable = matches!(config, SourceConfig::Url(_) | SourceConfig::Forgejo(_));
    let bytes = match source.fetch(&version).await {
        Ok(bytes) => {
            if cacheable {
                write_cache(fs.as_ref(), &cache_dir, &version, &bytes)
                    .await
                    .log_err();
            }
            bytes
        }
        Err(fetch_error) if cacheable => {
            let cached = fs.load_bytes(&cache_path(&cache_dir, &version)).await;
            match cached {
                Ok(bytes) => {
                    log::warn!("using the cached emulator module: {fetch_error:#}");
                    Arc::from(bytes)
                }
                Err(_) => return Err(fetch_error),
            }
        }
        Err(fetch_error) => return Err(fetch_error),
    };
    LoadedEmulator::compile(&bytes, label)
}

fn cache_path(cache_dir: &Path, version: &EmulatorVersion) -> PathBuf {
    let digest = Sha256::digest(version.id.as_bytes());
    let name: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    cache_dir.join(format!("{name}.wasm"))
}

async fn write_cache(
    fs: &dyn Fs,
    cache_dir: &Path,
    version: &EmulatorVersion,
    bytes: &[u8],
) -> Result<()> {
    fs.create_dir(cache_dir).await?;
    fs.write(&cache_path(cache_dir, version), bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::cell::Cell;
    use std::rc::Rc;

    fn setup(
        cx: &mut TestAppContext,
        config: Option<&str>,
    ) -> (Arc<fs::FakeFs>, Entity<EmuRuntime>) {
        let fs = fs::FakeFs::new(cx.executor());
        cx.foreground_executor()
            .block_test(fs.create_dir("/home/.ggo".as_ref()))
            .expect("dir");
        if let Some(config) = config {
            cx.foreground_executor()
                .block_test(fs.insert_file("/home/.ggo/emulator.json", config.as_bytes().to_vec()));
        }
        cx.update(|cx| {
            EmuRuntime::init_with_config_path(
                fs.clone(),
                http_client::FakeHttpClient::with_404_response(),
                "/home/.ggo/emulator.json".into(),
                "/cache".into(),
                cx,
            )
        });
        settle(cx);
        let runtime = cx.update(|cx| EmuRuntime::global(cx)).expect("global");
        (fs, runtime)
    }

    fn settle(cx: &mut TestAppContext) {
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn no_config_file_loads_the_bundled_module(cx: &mut TestAppContext) {
        let (_fs, runtime) = setup(cx, None);
        runtime.read_with(cx, |runtime, _| {
            assert_eq!(runtime.status(), RuntimeStatus::Ready);
            assert_eq!(runtime.current().expect("loaded").label.as_ref(), "Bundled");
        });
    }

    #[gpui::test]
    async fn rewriting_a_local_module_swaps_and_emits(cx: &mut TestAppContext) {
        let (fs, runtime) = setup(cx, Some(r#"{"source":{"path":"/work/ggo_emu.wasm"}}"#));
        fs.create_dir("/work".as_ref()).await.expect("work dir");
        fs.insert_file("/work/ggo_emu.wasm", BUNDLED_WASM.to_vec())
            .await;
        let changes = Rc::new(Cell::new(0));
        let _subscription = cx.update(|cx| {
            cx.subscribe(&runtime, {
                let changes = changes.clone();
                move |_, _: &EmulatorChanged, _| changes.set(changes.get() + 1)
            })
        });
        fs.insert_file("/work/ggo_emu.wasm", BUNDLED_WASM.to_vec())
            .await; // touch
        settle(cx);
        assert!(changes.get() >= 1);
        runtime.read_with(cx, |runtime, _| {
            assert!(
                runtime
                    .current()
                    .expect("loaded")
                    .label
                    .starts_with("Local")
            )
        });
    }

    #[gpui::test]
    async fn a_failing_source_keeps_the_previous_module_and_reports(cx: &mut TestAppContext) {
        let (fs, runtime) = setup(cx, None);
        fs.insert_file(
            "/home/.ggo/emulator.json",
            br#"{"source":{"url":"https://nowhere/ggo_emu.wasm"}}"#.to_vec(),
        )
        .await;
        settle(cx);
        runtime.read_with(cx, |runtime, _| {
            assert!(matches!(runtime.status(), RuntimeStatus::Failed(_)));
            assert_eq!(runtime.current().expect("kept").label.as_ref(), "Bundled");
        });
    }

    #[gpui::test]
    async fn a_malformed_config_reports_and_falls_back_to_bundled(cx: &mut TestAppContext) {
        let (_fs, runtime) = setup(cx, Some("{not json"));
        runtime.read_with(cx, |runtime, _| {
            assert!(
                matches!(runtime.status(), RuntimeStatus::Failed(message) if message.contains("emulator.json"))
            );
            assert!(runtime.current().is_some());
        });
    }

    #[gpui::test]
    async fn a_url_module_is_served_from_the_cache_when_the_fetch_fails(cx: &mut TestAppContext) {
        let (fs, runtime) = setup(cx, None);
        let version = EmulatorVersion {
            id: "https://nowhere/ggo_emu.wasm".into(),
            label: String::new(),
            download_url: None,
            prerelease: false,
        };
        fs.create_dir("/cache".as_ref()).await.expect("cache dir");
        fs.insert_file(
            cache_path(Path::new("/cache"), &version),
            BUNDLED_WASM.to_vec(),
        )
        .await;
        fs.insert_file(
            "/home/.ggo/emulator.json",
            br#"{"source":{"url":"https://nowhere/ggo_emu.wasm"}}"#.to_vec(),
        )
        .await;
        settle(cx);
        runtime.read_with(cx, |runtime, _| {
            assert_eq!(runtime.status(), RuntimeStatus::Ready);
            assert!(runtime.current().expect("cached").label.starts_with("URL"));
        });
    }

    #[test]
    fn config_json_shapes_match_the_spec() {
        let parse = |json: &str| {
            serde_json::from_str::<EmulatorConfig>(json)
                .expect(json)
                .source
        };
        assert_eq!(parse(r#"{"source":"bundled"}"#), SourceConfig::Bundled);
        assert_eq!(
            parse(r#"{"source":{"path":"/x.wasm"}}"#),
            SourceConfig::Path("/x.wasm".into())
        );
        assert_eq!(
            parse(r#"{"source":{"url":"https://h/x.wasm"}}"#),
            SourceConfig::Url("https://h/x.wasm".into())
        );
        assert!(matches!(
            parse(r#"{"source":{"forgejo":{"base_url":"https://g","owner":"o","repo":"r"}}}"#),
            SourceConfig::Forgejo(config) if config.tag == "latest" && config.asset == "ggo_emu.wasm"
        ));
    }
}
