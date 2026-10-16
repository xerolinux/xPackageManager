//! AppImage catalog. Pulls the AppImageHub feed (~1400 apps + their GitHub repo);
//! install resolves the repo's latest release to a downloadable `.AppImage`.

use std::process::Command;
use std::path::PathBuf;
use xpm_core::error::{Error, Result};

pub const FEED_URL: &str = "https://appimage.github.io/feed.json";
pub const ICON_BASE: &str = "https://appimage.github.io/database/";

/// A browsable catalog app (only entries with a resolvable GitHub repo).
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    pub categories: Vec<String>,
    /// GitHub "owner/repo" - used to resolve the download.
    pub github: String,
    pub icon_url: Option<String>,
    /// Name of the catalog source this entry came from (set by fetch_sources_named).
    pub source: String,
}

/// Feed descriptions often contain raw AppStream HTML (`<p>`, `<ul>`, `<li>`)
/// with newlines. Strip tags, decode common entities, collapse whitespace to a
/// single line, and cap length so catalog rows stay one tidy line.
fn sanitize_description(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if in_tag => {}
            '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'");
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 160;
    if collapsed.chars().count() > MAX {
        let truncated: String = collapsed.chars().take(MAX).collect();
        format!("{}…", truncated.trim_end())
    } else {
        collapsed
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(cmd)).find(|p| p.is_file())
}

/// Optional GitHub API token. Unauthenticated requests are capped at 60/hr per IP,
/// which a library of a few dozen apps exhausts on a single update check. A token
/// raises the limit to 5000/hr. Set once at startup and on settings change.
static GITHUB_TOKEN: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Set (or clear with an empty/None value) the GitHub API token used for release
/// resolution and update checks. Process-global; safe to call from any thread.
pub fn set_github_token(token: Option<String>) {
    let cleaned = token.and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() { None } else { Some(t) }
    });
    if let Ok(mut w) = GITHUB_TOKEN.write() {
        *w = cleaned;
    }
}

fn github_token() -> Option<String> {
    GITHUB_TOKEN.read().ok().and_then(|g| g.clone())
}

fn is_github_api(url: &str) -> bool {
    url.contains("api.github.com")
}

fn curl_text(url: &str, accept: Option<&str>) -> Result<String> {
    if which("curl").is_none() {
        return Err(Error::Other("curl is required".to_string()));
    }
    let mut cmd = Command::new("curl");
    cmd.args(["-fsSL", "--max-time", "30"]);
    if let Some(a) = accept {
        cmd.args(["-H", &format!("Accept: {}", a)]);
    }
    cmd.args(["-H", "User-Agent: xPackageManager"]);
    let authed = if is_github_api(url) {
        if let Some(tok) = github_token() {
            cmd.args(["-H", &format!("Authorization: Bearer {}", tok)]);
            true
        } else {
            false
        }
    } else {
        false
    };
    cmd.arg(url);
    let out = cmd.output().map_err(|e| Error::NetworkError(e.to_string()))?;
    if !out.status.success() {
        if is_github_api(url) && !authed {
            return Err(Error::NetworkError(format!(
                "GitHub request failed (likely unauthenticated rate limit - add a GitHub token in AppImage settings): {}",
                url
            )));
        }
        return Err(Error::NetworkError(format!("Request failed: {}", url)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Fetch + merge feed-JSON sources, de-duped by GitHub repo then name. Per-source
/// errors are logged and skipped.
pub fn fetch_sources(urls: &[String]) -> Vec<CatalogEntry> {
    let named: Vec<(String, String)> = urls.iter().map(|u| (u.clone(), u.clone())).collect();
    fetch_sources_named(&named)
}

/// Like `fetch_sources` but tags each entry with the source NAME it came from
/// (so the UI can offer a per-source dropdown). `sources` is (name, url) pairs.
pub fn fetch_sources_named(sources: &[(String, String)]) -> Vec<CatalogEntry> {
    let mut merged: Vec<CatalogEntry> = Vec::new();
    let mut seen_repo = std::collections::HashSet::new();
    let mut seen_name = std::collections::HashSet::new();

    for (name, url) in sources {
        if url.trim().is_empty() {
            continue;
        }
        match fetch_one(url) {
            Ok(entries) => {
                for mut e in entries {
                    let repo_key = e.github.to_lowercase();
                    let name_key = e.name.to_lowercase();
                    if !seen_repo.insert(repo_key) || !seen_name.insert(name_key) {
                        continue;
                    }
                    e.source = name.clone();
                    merged.push(e);
                }
            }
            Err(err) => tracing::warn!("AppImage source failed ({}): {}", url, err),
        }
    }
    merged.sort_by_key(|e| e.name.to_lowercase());
    merged
}

const FEED_CACHE_SECS: u64 = 6 * 3600;

fn feed_cache_path(url: &str) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            PathBuf::from(home).join(".cache")
        });
    let slug: String = url.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    base.join("xpm/appimage-feeds").join(format!("{}.json", slug))
}

/// Delete the on-disk feed cache for the given sources so the next fetch hits the
/// network. Used by the "Reload List" button when a stale/partial cache leaves a
/// user stuck on "cannot load catalog".
pub fn clear_feed_cache(urls: &[String]) {
    for url in urls {
        if url.trim().is_empty() {
            continue;
        }
        let _ = std::fs::remove_file(feed_cache_path(url));
    }
}

/// Return the feed body from a fresh on-disk cache, else download and cache it.
/// Disk cache makes repeat page loads instant (no network round-trip).
fn cached_feed_body(url: &str) -> Result<String> {
    let cache = feed_cache_path(url);
    if let Ok(meta) = std::fs::metadata(&cache) {
        let fresh = meta
            .modified()
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map(|age| age.as_secs() < FEED_CACHE_SECS)
            .unwrap_or(false);
        if fresh
            && let Ok(body) = std::fs::read_to_string(&cache) {
                return Ok(body);
            }
    }
    let body = curl_text(url, Some("application/json"))?;
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&cache, &body);
    Ok(body)
}

/// Fetch and parse a single feed-JSON source (disk-cached).
fn fetch_one(url: &str) -> Result<Vec<CatalogEntry>> {
    let body = cached_feed_body(url)?;
    let root: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| Error::Other(format!("Bad feed JSON: {}", e)))?;

    let items = root
        .get("items")
        .and_then(|v| v.as_array())
        .ok_or_else(|| Error::Other("Feed has no items".to_string()))?;

    let mut entries: Vec<CatalogEntry> = Vec::new();
    for item in items {
        let name = match item.get("name").and_then(|v| v.as_str()) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };

        let github = item
            .get("links")
            .and_then(|v| v.as_array())
            .and_then(|links| {
                links.iter().find_map(|l| {
                    if l.get("type").and_then(|t| t.as_str()) == Some("GitHub") {
                        l.get("url").and_then(|u| u.as_str()).filter(|u| u.contains('/')).map(String::from)
                    } else {
                        None
                    }
                })
            });
        let Some(github) = github else { continue };

        let description = sanitize_description(
            item.get("description").and_then(|v| v.as_str()).unwrap_or_default(),
        );

        let categories: Vec<String> = item
            .get("categories")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let icon_url = item
            .get("icons")
            .and_then(|v| v.as_array())
            .and_then(|a| a.iter().find_map(|i| i.as_str()))
            .map(|i| {
                if i.starts_with("http://") || i.starts_with("https://") {
                    i.to_string()
                } else {
                    format!("{}{}", ICON_BASE, i)
                }
            });

        entries.push(CatalogEntry { name, description, categories, github, icon_url, source: String::new() });
    }
    entries.sort_by_key(|e| e.name.to_lowercase());
    Ok(entries)
}

/// Resolve a GitHub "owner/repo" to a direct `.AppImage` download URL from its
/// latest release. Prefers an x86_64/amd64 asset.
pub fn resolve_download(github: &str) -> Result<String> {
    let api = format!("https://api.github.com/repos/{}/releases/latest", github.trim_matches('/'));
    let body = curl_text(&api, Some("application/vnd.github+json"))?;
    let release: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| Error::Other(format!("Bad release JSON: {}", e)))?;

    let assets = release
        .get("assets")
        .and_then(|a| a.as_array())
        .ok_or_else(|| Error::Other("No release assets found".to_string()))?;

    let urls: Vec<(String, String)> = assets
        .iter()
        .filter_map(|a| {
            let name = a.get("name")?.as_str()?.to_string();
            let url = a.get("browser_download_url")?.as_str()?.to_string();
            if name.to_lowercase().ends_with(".appimage") {
                Some((name, url))
            } else {
                None
            }
        })
        .collect();

    if urls.is_empty() {
        return Err(Error::Other(format!(
            "No .AppImage asset in the latest release of {}",
            github
        )));
    }
    let is_other_arch = |n: &str| {
        let l = n.to_lowercase();
        ["i386", "i686", "aarch64", "arm64", "armv7l", "armhf"]
            .iter()
            .any(|a| l.contains(a))
    };
    let pool: Vec<&(String, String)> = {
        let kept: Vec<&(String, String)> =
            urls.iter().filter(|(n, _)| !is_other_arch(n)).collect();
        if kept.is_empty() {
            urls.iter().collect()
        } else {
            kept
        }
    };
    let preferred = pool.iter().find(|(n, _)| {
        let l = n.to_lowercase();
        l.contains("x86_64") || l.contains("amd64") || l.contains("x86-64")
    });
    Ok(preferred.unwrap_or(&pool[0]).1.clone())
}
