//! GGO: small per-file thumbnails for the project panel. Fork-local
//! panels register a decoder for their extensions (`.til`, `.spr`,
//! `.png`); the project panel asks the cache for a file's image while
//! rendering a row and re-renders when a decode lands. This file carries
//! no image-format knowledge: decoders build the `RenderImage` themselves,
//! off the UI thread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{App, AppContext as _, Context, Entity, Global, RenderImage};

/// Runs on a background thread: read + decode + downscale `path`, or
/// `None` when it cannot (the row keeps its file icon).
pub type ThumbnailDecoder = fn(&Path) -> Option<Arc<RenderImage>>;

/// Thumbnails kept before the oldest is evicted.
const CAPACITY: usize = 512;

struct Registered {
    extensions: &'static [&'static str],
    decoder: ThumbnailDecoder,
}

#[derive(Default)]
struct Registry {
    decoders: Vec<Registered>,
    cache: Option<Entity<ThumbnailCache>>,
}

impl Global for Registry {}

/// Registers `decoder` for files whose (lower-cased) extension is in
/// `extensions`.
pub fn register_thumbnail_decoder(
    cx: &mut App,
    extensions: &'static [&'static str],
    decoder: ThumbnailDecoder,
) {
    cx.default_global::<Registry>().decoders.push(Registered {
        extensions,
        decoder,
    });
}

/// The app's one thumbnail cache; `None` until a decoder is registered,
/// so an app without GGO panels never builds one.
pub fn thumbnail_cache(cx: &mut App) -> Option<Entity<ThumbnailCache>> {
    let registry = cx.try_global::<Registry>()?;
    if registry.decoders.is_empty() {
        return None;
    }
    if let Some(cache) = &registry.cache {
        return Some(cache.clone());
    }
    let cache = cx.new(|_| ThumbnailCache::default());
    cx.default_global::<Registry>().cache = Some(cache.clone());
    Some(cache)
}

fn decoder_for(path: &Path, cx: &App) -> Option<ThumbnailDecoder> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    cx.try_global::<Registry>()?
        .decoders
        .iter()
        .find(|registered| registered.extensions.contains(&extension.as_str()))
        .map(|registered| registered.decoder)
}

/// How long a hit is trusted before the file is stat'd again.
const RECHECK_AFTER: Duration = Duration::from_secs(2);

struct CacheEntry {
    mtime: u64,
    image: Option<Arc<RenderImage>>,
    checked: Instant,
}

#[derive(Default)]
pub struct ThumbnailCache {
    entries: HashMap<PathBuf, CacheEntry>,
    pending: HashSet<PathBuf>,
    /// Insertion order, for eviction.
    order: VecDeque<PathBuf>,
}

fn mtime_of(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

impl ThumbnailCache {
    /// The thumbnail for `path` if one is cached for its current mtime.
    /// A miss (or a stale hit) starts a background decode and returns
    /// whatever is cached meanwhile; the cache notifies when it lands.
    pub fn get(&mut self, path: &Path, cx: &mut Context<Self>) -> Option<Arc<RenderImage>> {
        let decoder = decoder_for(path, cx)?;
        // Rows re-render often; a stat per row per frame is the cost to
        // avoid, so a fresh hit skips it.
        if let Some(entry) = self.entries.get(path)
            && entry.checked.elapsed() < RECHECK_AFTER
        {
            return entry.image.clone();
        }
        let mtime = mtime_of(path)?;
        if let Some(entry) = self.entries.get_mut(path)
            && entry.mtime == mtime
        {
            entry.checked = Instant::now();
            return entry.image.clone();
        }
        if self.pending.insert(path.to_path_buf()) {
            let path = path.to_path_buf();
            cx.spawn(async move |this, cx| {
                let image = cx
                    .background_spawn({
                        let path = path.clone();
                        async move { decoder(&path) }
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.pending.remove(&path);
                    this.insert(
                        path,
                        CacheEntry {
                            mtime,
                            image,
                            checked: Instant::now(),
                        },
                        cx,
                    );
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
        self.entries.get(path).and_then(|entry| entry.image.clone())
    }

    fn insert(&mut self, path: PathBuf, entry: CacheEntry, cx: &mut Context<Self>) {
        match self.entries.insert(path.clone(), entry) {
            Some(old) => {
                if let Some(image) = old.image {
                    cx.drop_image(image, None);
                }
            }
            None => self.order.push_back(path),
        }
        while self.order.len() > CAPACITY {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest)
                && let Some(image) = evicted.image
            {
                cx.drop_image(image, None);
            }
        }
    }

    pub fn is_cached(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }
}
