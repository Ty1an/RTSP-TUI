use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const APP_DIR: &str = "rtsp-tui";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredStream {
    pub url: String,
    pub status: u16,
    pub discovered_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DiscoveryCache {
    pub streams: Vec<DiscoveredStream>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SelectedStream {
    pub base_url: String,
    #[serde(default = "default_selected_true")]
    pub enabled: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SelectionCache {
    pub streams: Vec<SelectedStream>,
}

impl DiscoveryCache {
    #[must_use]
    pub fn sort_and_dedup(mut self) -> Self {
        self.streams.sort_by(|a, b| a.url.cmp(&b.url));
        self.streams.dedup_by(|a, b| a.url == b.url);
        self
    }
}

pub fn cache_path() -> Result<PathBuf> {
    scoped_path("discovered_streams.json", APP_DIR)
}

pub fn selection_cache_path() -> Result<PathBuf> {
    scoped_path("selected_streams.json", APP_DIR)
}

pub fn load_cache() -> Result<DiscoveryCache> {
    let path = cache_path()?;
    if !path.exists() {
        return Ok(DiscoveryCache::default());
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading discovery cache at {}", path.display()))?;
    let parsed = serde_json::from_str::<DiscoveryCache>(&raw)
        .with_context(|| format!("failed parsing discovery cache at {}", path.display()))?;
    Ok(parsed)
}

pub fn save_cache(cache: &DiscoveryCache) -> Result<()> {
    let path = cache_path()?;
    ensure_parent_dir(&path)?;

    let payload =
        serde_json::to_string_pretty(cache).context("failed serializing discovery cache")?;
    fs::write(&path, payload)
        .with_context(|| format!("failed writing discovery cache at {}", path.display()))?;
    Ok(())
}

pub fn load_selection_cache() -> Result<SelectionCache> {
    let path = selection_cache_path()?;
    if !path.exists() {
        return Ok(SelectionCache::default());
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading selection cache at {}", path.display()))?;
    let parsed = serde_json::from_str::<SelectionCache>(&raw)
        .with_context(|| format!("failed parsing selection cache at {}", path.display()))?;
    Ok(parsed)
}

pub fn save_selection_cache(cache: &SelectionCache) -> Result<()> {
    let path = selection_cache_path()?;
    ensure_parent_dir(&path)?;

    let payload =
        serde_json::to_string_pretty(cache).context("failed serializing selection cache")?;
    fs::write(&path, payload)
        .with_context(|| format!("failed writing selection cache at {}", path.display()))?;
    Ok(())
}

#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const fn default_selected_true() -> bool {
    true
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating cache directory {}", parent.display()))?;
    }
    Ok(())
}

fn data_root() -> Result<PathBuf> {
    dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .context("unable to determine user data directory")
}

fn scoped_path(file: &str, app_dir: &str) -> Result<PathBuf> {
    Ok(data_root()?.join(app_dir).join(file))
}
