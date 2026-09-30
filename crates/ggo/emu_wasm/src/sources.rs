//! Where the emulator module (`ggo_emu.wasm`) bytes come from.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail, ensure};
use fs::Fs;
use futures::AsyncReadExt as _;
use futures::future::BoxFuture;
use gpui::SharedString;
use http_client::{AsyncBody, HttpClient, HttpRequestExt as _};
use serde::{Deserialize, Serialize};

use crate::BUNDLED_WASM;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmulatorVersion {
    pub id: String,
    pub label: String,
    pub download_url: Option<String>,
    pub prerelease: bool,
}

pub trait EmulatorSource: Send + Sync {
    fn describe(&self) -> SharedString;
    fn list_versions(&self) -> BoxFuture<'static, Result<Vec<EmulatorVersion>>>;
    fn fetch(&self, version: &EmulatorVersion) -> BoxFuture<'static, Result<Arc<[u8]>>>;
}

pub struct BundledSource;

impl EmulatorSource for BundledSource {
    fn describe(&self) -> SharedString {
        "Bundled".into()
    }

    fn list_versions(&self) -> BoxFuture<'static, Result<Vec<EmulatorVersion>>> {
        Box::pin(async {
            Ok(vec![EmulatorVersion {
                id: "bundled".into(),
                label: "Bundled".into(),
                download_url: None,
                prerelease: false,
            }])
        })
    }

    fn fetch(&self, _version: &EmulatorVersion) -> BoxFuture<'static, Result<Arc<[u8]>>> {
        Box::pin(async { Ok(Arc::from(BUNDLED_WASM)) })
    }
}

pub struct LocalSource {
    pub path: PathBuf,
    pub fs: Arc<dyn Fs>,
}

impl EmulatorSource for LocalSource {
    fn describe(&self) -> SharedString {
        format!("Local: {}", self.path.display()).into()
    }

    fn list_versions(&self) -> BoxFuture<'static, Result<Vec<EmulatorVersion>>> {
        let id = self.path.display().to_string();
        Box::pin(async move {
            Ok(vec![EmulatorVersion {
                label: id.clone(),
                id,
                download_url: None,
                prerelease: false,
            }])
        })
    }

    fn fetch(&self, _version: &EmulatorVersion) -> BoxFuture<'static, Result<Arc<[u8]>>> {
        let path = self.path.clone();
        let fs = self.fs.clone();
        Box::pin(async move {
            let bytes = fs
                .load_bytes(&path)
                .await
                .with_context(|| format!("reading {}", path.display()))?;
            Ok(Arc::from(bytes))
        })
    }
}

pub struct HttpSource {
    pub url: String,
    pub http: Arc<dyn HttpClient>,
}

impl EmulatorSource for HttpSource {
    fn describe(&self) -> SharedString {
        format!("URL: {}", self.url).into()
    }

    fn list_versions(&self) -> BoxFuture<'static, Result<Vec<EmulatorVersion>>> {
        let url = self.url.clone();
        Box::pin(async move {
            Ok(vec![EmulatorVersion {
                id: url.clone(),
                label: url.clone(),
                download_url: Some(url),
                prerelease: false,
            }])
        })
    }

    fn fetch(&self, version: &EmulatorVersion) -> BoxFuture<'static, Result<Arc<[u8]>>> {
        let http = self.http.clone();
        let url = version
            .download_url
            .clone()
            .unwrap_or_else(|| self.url.clone());
        Box::pin(async move { Ok(Arc::from(get_bytes(http, url, None).await?)) })
    }
}

fn default_asset() -> String {
    "ggo_emu.wasm".into()
}

fn default_tag() -> String {
    "latest".into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgejoConfig {
    pub base_url: String,
    pub owner: String,
    pub repo: String,
    #[serde(default = "default_asset")]
    pub asset: String,
    #[serde(default = "default_tag")]
    pub tag: String,
    #[serde(default)]
    pub token: Option<String>,
}

pub struct ForgejoSource {
    pub config: ForgejoConfig,
    pub http: Arc<dyn HttpClient>,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

impl ForgejoSource {
    pub fn resolve(&self, versions: &[EmulatorVersion]) -> Result<EmulatorVersion> {
        let config = &self.config;
        let found = if config.tag == "latest" {
            versions.iter().find(|version| !version.prerelease)
        } else {
            let suffix = format!("@{}", config.tag);
            versions
                .iter()
                .find(|version| version.id.ends_with(&suffix))
        };
        match found {
            Some(version) => Ok(version.clone()),
            None => bail!(
                "no release {} with asset {} in {}/{}",
                config.tag,
                config.asset,
                config.owner,
                config.repo
            ),
        }
    }
}

impl EmulatorSource for ForgejoSource {
    fn describe(&self) -> SharedString {
        let config = &self.config;
        format!("Forgejo: {}/{}@{}", config.owner, config.repo, config.tag).into()
    }

    fn list_versions(&self) -> BoxFuture<'static, Result<Vec<EmulatorVersion>>> {
        let config = self.config.clone();
        let http = self.http.clone();
        Box::pin(async move {
            let base_url = config.base_url.trim_end_matches('/');
            let url = format!(
                "{base_url}/api/v1/repos/{}/{}/releases?limit=50",
                config.owner, config.repo
            );
            let body = get_bytes(http, url, config.token.clone()).await?;
            let releases: Vec<Release> =
                serde_json::from_slice(&body).context("parsing Forgejo releases")?;
            Ok(releases
                .into_iter()
                .filter(|release| !release.draft)
                .filter_map(|release| {
                    let asset = release
                        .assets
                        .into_iter()
                        .find(|asset| asset.name == config.asset)?;
                    let label = if release.prerelease {
                        format!("{} (pre-release)", release.tag_name)
                    } else {
                        release.tag_name.clone()
                    };
                    Some(EmulatorVersion {
                        id: format!(
                            "{base_url}/{}/{}@{}",
                            config.owner, config.repo, release.tag_name
                        ),
                        label,
                        download_url: Some(asset.browser_download_url),
                        prerelease: release.prerelease,
                    })
                })
                .collect())
        })
    }

    fn fetch(&self, version: &EmulatorVersion) -> BoxFuture<'static, Result<Arc<[u8]>>> {
        let http = self.http.clone();
        let token = self.config.token.clone();
        let url = version.download_url.clone();
        Box::pin(async move {
            let url = url.context("release has no download URL")?;
            Ok(Arc::from(get_bytes(http, url, token).await?))
        })
    }
}

async fn get_bytes(
    http: Arc<dyn HttpClient>,
    url: String,
    token: Option<String>,
) -> Result<Vec<u8>> {
    let mut request = http_client::Request::builder()
        .method(http_client::Method::GET)
        .uri(&url);
    if let Some(token) = token {
        request = request.header("Authorization", format!("token {token}"));
    }
    let request = request
        .follow_redirects(http_client::RedirectPolicy::FollowAll)
        .body(AsyncBody::empty())?;
    let mut response = http
        .send(request)
        .await
        .with_context(|| format!("GET {url}"))?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    ensure!(
        response.status().is_success(),
        "GET {url}: HTTP {}",
        response.status()
    );
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::FakeHttpClient;

    const RELEASES: &str = r#"[
      {"tag_name":"v0.3.0-rc1","draft":false,"prerelease":true,
       "assets":[{"name":"ggo_emu.wasm","browser_download_url":"https://git.example/dl/rc1.wasm"}]},
      {"tag_name":"v0.2.0","draft":false,"prerelease":false,
       "assets":[{"name":"ggo_emu.wasm","browser_download_url":"https://git.example/dl/v020.wasm"}]},
      {"tag_name":"v0.1.9","draft":true,"prerelease":false,
       "assets":[{"name":"ggo_emu.wasm","browser_download_url":"https://git.example/dl/draft.wasm"}]},
      {"tag_name":"v0.1.0","draft":false,"prerelease":false,
       "assets":[{"name":"other.bin","browser_download_url":"https://git.example/dl/other.bin"}]}
    ]"#;

    fn forgejo(tag: &str) -> ForgejoSource {
        let http = FakeHttpClient::create(|request| async move {
            let uri = request.uri().to_string();
            let body = match uri.as_str() {
                "https://git.example/api/v1/repos/gemdrop/ggo/releases?limit=50" => RELEASES.as_bytes().to_vec(),
                "https://git.example/dl/v020.wasm" => b"\0asmV020".to_vec(),
                _ => return Ok(http_client::Response::builder().status(404).body(Default::default())?),
            };
            Ok(http_client::Response::builder().status(200).body(body.into())?)
        });
        ForgejoSource {
            config: ForgejoConfig { base_url: "https://git.example".into(), owner: "gemdrop".into(), repo: "ggo".into(),
                asset: "ggo_emu.wasm".into(), tag: tag.into(), token: None },
            http,
        }
    }

    #[test]
    fn forgejo_lists_non_draft_releases_that_carry_the_asset() {
        let versions = futures::executor::block_on(forgejo("latest").list_versions()).expect("list");
        let labels: Vec<_> = versions.iter().map(|v| v.label.as_str()).collect();
        assert_eq!(labels, ["v0.3.0-rc1 (pre-release)", "v0.2.0"]);
    }

    #[test]
    fn latest_skips_pre_releases_and_fetches_the_asset() {
        let source = forgejo("latest");
        let versions = futures::executor::block_on(source.list_versions()).expect("list");
        let version = source.resolve(&versions).expect("resolve");
        assert_eq!(version.id, "https://git.example/gemdrop/ggo@v0.2.0");
        let bytes = futures::executor::block_on(source.fetch(&version)).expect("fetch");
        assert_eq!(&bytes[..], b"\0asmV020");
    }

    #[test]
    fn a_pinned_tag_that_does_not_exist_is_an_error() {
        let source = forgejo("v9.9.9");
        let versions = futures::executor::block_on(source.list_versions()).expect("list");
        assert!(source.resolve(&versions).is_err());
    }

    #[test]
    fn http_source_fetches_its_url_and_reports_http_errors() {
        let http = FakeHttpClient::create(|request| async move {
            let status = if request.uri().path() == "/ok.wasm" { 200 } else { 500 };
            Ok(http_client::Response::builder().status(status).body(b"\0asm".to_vec().into())?)
        });
        let ok = HttpSource { url: "https://host/ok.wasm".into(), http: http.clone() };
        let version = futures::executor::block_on(ok.list_versions()).expect("list").remove(0);
        assert_eq!(&futures::executor::block_on(ok.fetch(&version)).expect("fetch")[..], b"\0asm");
        let bad = HttpSource { url: "https://host/bad.wasm".into(), http };
        let version = futures::executor::block_on(bad.list_versions()).expect("list").remove(0);
        assert!(futures::executor::block_on(bad.fetch(&version)).is_err());
    }

    #[gpui::test]
    async fn local_source_reads_its_file(cx: &mut gpui::TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        fs.create_dir("/work".as_ref()).await.expect("mkdir");
        fs.insert_file("/work/ggo_emu.wasm", b"\0asmLOCAL".to_vec()).await;
        let source = LocalSource { path: "/work/ggo_emu.wasm".into(), fs };
        let version = source.list_versions().await.expect("list").remove(0);
        assert_eq!(&source.fetch(&version).await.expect("fetch")[..], b"\0asmLOCAL");
    }
}
