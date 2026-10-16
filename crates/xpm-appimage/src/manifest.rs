use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One tracked AppImage. Persisted in manifest.json under the data dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppImageEntry {
    /// Sanitized id used for file names and removal lookups.
    pub name: String,
    /// Human-facing name pulled from the bundled .desktop (falls back to name).
    pub display_name: String,
    pub version: String,
    /// Absolute path to the stored, executable AppImage.
    pub path: String,
    /// Where it came from (URL for remote installs, original path for local).
    #[serde(default)]
    pub source_url: Option<String>,
    /// Catalog GitHub "owner/repo" if installed from the catalog (for matching).
    #[serde(default)]
    pub github: Option<String>,
    /// Raw contents of the ELF `.upd_info` section, if any.
    #[serde(default)]
    pub update_info: Option<String>,
    /// True when the AppImage carries embedded update information (zsync/gh-releases).
    #[serde(default)]
    pub supports_update: bool,
    /// Path to the integrated icon copied into the icon theme dir.
    #[serde(default)]
    pub icon_path: Option<String>,
    /// Path to the generated .desktop entry in the applications dir.
    #[serde(default)]
    pub desktop_path: Option<String>,
    #[serde(default)]
    pub size: u64,
}

fn data_home() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.is_empty() {
            return PathBuf::from(xdg);
        }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/share")
}

/// `~/.local/share/xpm/appimages` - where stored AppImages and the manifest live.
pub fn store_dir() -> PathBuf {
    data_home().join("xpm/appimages")
}

/// `~/.local/share/applications` - XDG desktop entries.
pub fn applications_dir() -> PathBuf {
    data_home().join("applications")
}

/// `~/.local/share/icons` - integrated icons.
pub fn icons_dir() -> PathBuf {
    data_home().join("icons")
}

pub fn manifest_path() -> PathBuf {
    store_dir().join("manifest.json")
}

/// Read the manifest from disk. Missing file => empty list. A present-but-corrupt
/// file is backed up (not silently dropped) so a later save can't clobber it.
pub fn load() -> Vec<AppImageEntry> {
    let path = manifest_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    match serde_json::from_str(&content) {
        Ok(entries) => entries,
        Err(e) => {
            tracing::error!("AppImage manifest corrupt ({}); backing up", e);
            let _ = std::fs::rename(&path, path.with_extension("json.corrupt"));
            Vec::new()
        }
    }
}

/// Persist the manifest atomically (write temp + rename) so an interrupted or
/// crashed write can never leave a truncated/empty manifest.
pub fn save(entries: &[AppImageEntry]) -> std::io::Result<()> {
    let dir = store_dir();
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(entries)
        .unwrap_or_else(|_| "[]".to_string());
    let path = manifest_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// Turn an arbitrary display/file name into a safe slug for file names and ids.
pub fn sanitize_name(raw: &str) -> String {
    let stem = raw
        .trim_end_matches(".AppImage")
        .trim_end_matches(".appimage");
    let slug: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() { "appimage".to_string() } else { slug }
}
