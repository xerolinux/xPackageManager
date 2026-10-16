use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;

const API: &str = "https://flathub.org/api/v2";

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Extras {
    pub download_size: u64,
    pub installed_size: u64,
    pub installs: u64,
    pub verified: bool,
}

fn get(path: &str) -> Option<Value> {
    let out = Command::new("curl")
        .args(["-sf", "--max-time", "8", &format!("{API}/{path}")])
        .output()
        .ok()?;
    out.status.success().then(|| serde_json::from_slice(&out.stdout).ok()).flatten()
}

fn sizes(summary: &Value) -> (u64, u64) {
    let field = |name: &str| summary.get(name).and_then(Value::as_u64).unwrap_or(0);
    (field("download_size"), field("installed_size"))
}

fn verified(status: &Value) -> bool {
    status.get("verified").and_then(Value::as_bool).unwrap_or(false)
}

fn installs(stats: &Value) -> u64 {
    stats.get("installs_total").and_then(Value::as_u64).unwrap_or(0)
}

/// Download/installed size, verification and install count from the Flathub API.
/// `None` when nothing could be fetched (offline, unknown app).
pub(crate) fn fetch_extras(app_id: &str) -> Option<Extras> {
    let (summary, status, stats) = std::thread::scope(|scope| {
        let summary = scope.spawn(|| get(&format!("summary/{app_id}")));
        let status = scope.spawn(|| get(&format!("verification/{app_id}/status")));
        let stats = scope.spawn(|| get(&format!("stats/{app_id}")));
        (summary.join().ok().flatten(), status.join().ok().flatten(), stats.join().ok().flatten())
    });
    if summary.is_none() && status.is_none() && stats.is_none() {
        return None;
    }
    let (download_size, installed_size) = summary.as_ref().map(sizes).unwrap_or_default();
    Some(Extras {
        download_size,
        installed_size,
        installs: stats.as_ref().map(installs).unwrap_or(0),
        verified: status.as_ref().map(verified).unwrap_or(false),
    })
}

pub(crate) fn format_count(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

pub(crate) fn screenshot_path(app_id: &str, index: usize, url: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let ext = if url.rsplit('.').next().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg")) {
        "jpg"
    } else {
        "png"
    };
    PathBuf::from(home)
        .join(".cache/xpm/flatpak-screenshots")
        .join(app_id.replace(['/', '\\'], "_"))
        .join(format!("{index}.{ext}"))
}

/// Download `url` to `dest` (atomically, 20 s timeout). Existing files are reused.
pub(crate) fn download_screenshot(url: &str, dest: &PathBuf) -> bool {
    if dest.is_file() {
        return true;
    }
    if let Some(dir) = dest.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let part = dest.with_extension("part");
    let ok = Command::new("curl")
        .args(["-sfL", "--max-time", "20", "-o"])
        .arg(&part)
        .arg(url)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && std::fs::rename(&part, dest).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(&part);
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_summary_sizes() {
        let v = json!({"download_size": 129731811u64, "installed_size": 345120768u64, "branch": "stable"});
        assert_eq!(sizes(&v), (129_731_811, 345_120_768));
        assert_eq!(sizes(&json!({})), (0, 0));
    }

    #[test]
    fn parses_verification_and_installs() {
        assert!(verified(&json!({"verified": true, "method": "manual"})));
        assert!(!verified(&json!({"verified": false})));
        assert!(!verified(&json!({})));
        assert_eq!(installs(&json!({"installs_total": 12399708, "installs_per_day": {}})), 12_399_708);
    }

    #[test]
    fn formats_install_counts() {
        assert_eq!(format_count(950), "950");
        assert_eq!(format_count(12_345), "12.3K");
        assert_eq!(format_count(12_399_708), "12.4M");
    }

    #[test]
    fn screenshot_cache_paths_are_per_app_and_index() {
        let a = screenshot_path("org.x.App", 0, "https://img/a.png");
        let b = screenshot_path("org.x.App", 1, "https://img/b.jpg");
        assert!(a.ends_with("org.x.App/0.png"));
        assert!(b.ends_with("org.x.App/1.jpg"));
    }
}
