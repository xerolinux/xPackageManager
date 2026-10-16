//! Desktop-menu integration for installed AppImages.
//!
//! Extracts the bundled `.desktop` and icon from an AppImage and writes a
//! patched desktop entry into `~/.local/share/applications` so the app shows
//! up in the launcher/menu. Best-effort: integration failures never abort an
//! install - the AppImage stays runnable, it just may lack a menu entry/icon.

use crate::manifest;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::warn;

pub struct Integration {
    pub display_name: String,
    pub version: Option<String>,
    pub icon_path: Option<String>,
    pub desktop_path: Option<String>,
}

/// Extract `.desktop` + icon, write a launcher entry. `name` is the sanitized id.
pub fn integrate(appimage_path: &Path, name: &str, fallback_display: &str) -> Integration {
    let mut result = Integration {
        display_name: fallback_display.to_string(),
        version: None,
        icon_path: None,
        desktop_path: None,
    };

    let temp = manifest::store_dir().join(format!(".extract-{}", name));
    let _ = std::fs::remove_dir_all(&temp);
    if std::fs::create_dir_all(&temp).is_err() {
        warn!("appimage: cannot create temp extract dir");
        result.desktop_path = write_desktop(
            appimage_path, name, &result.display_name, &result.icon_path, "Utility;",
        );
        refresh_desktop_db();
        return result;
    }

    extract_all(appimage_path, &temp);
    let root = temp.join("squashfs-root");

    let mut icon_name: Option<String> = None;
    let mut categories = "Utility;".to_string();
    if let Some(desktop_src) = find_desktop(&root)
        && let Ok(content) = std::fs::read_to_string(&desktop_src) {
            if let Some(n) = desktop_value(&content, "Name").filter(|s| !s.is_empty()) {
                result.display_name = n;
            }
            if let Some(v) = desktop_value(&content, "X-AppImage-Version").filter(|s| !s.is_empty()) {
                result.version = Some(v);
            }
            icon_name = desktop_value(&content, "Icon").filter(|s| !s.is_empty());
            if let Some(c) = desktop_value(&content, "Categories").filter(|s| !s.is_empty()) {
                categories = c;
            }
        }

    if let Some(icon_dest) = copy_icon(&root, name, icon_name.as_deref()) {
        result.icon_path = Some(icon_dest);
    }

    result.desktop_path = write_desktop(
        appimage_path, name, &result.display_name, &result.icon_path, &categories,
    );

    let _ = std::fs::remove_dir_all(&temp);
    refresh_desktop_db();
    result
}

/// Remove the launcher entry and icon created for `name`.
pub fn deintegrate(icon_path: &Option<String>, desktop_path: &Option<String>) {
    if let Some(d) = desktop_path {
        let _ = std::fs::remove_file(d);
    }
    if let Some(i) = icon_path {
        let _ = std::fs::remove_file(i);
    }
    refresh_desktop_db();
}

fn extract_all(appimage: &Path, cwd: &Path) {
    let _ = Command::new(appimage)
        .arg("--appimage-extract")
        .current_dir(cwd)
        .output();
}

/// Recursively collect files under `dir` (following into subdirs), up to a cap.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>, depth: u32) {
    if depth > 8 || out.len() > 20000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let meta = match std::fs::metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            collect_files(&p, out, depth + 1);
        } else {
            out.push(p);
        }
    }
}

/// Find the app's .desktop: prefer root-level, else usr/share/applications.
fn find_desktop(root: &Path) -> Option<PathBuf> {
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "desktop").unwrap_or(false)
                && std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false)
            {
                return Some(p);
            }
        }
    }
    let apps = root.join("usr/share/applications");
    if let Ok(entries) = std::fs::read_dir(&apps) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map(|x| x == "desktop").unwrap_or(false) {
                return Some(p);
            }
        }
    }
    None
}

/// Locate the best icon file inside the extracted tree and copy it into the icon
/// dir. Order: resolved .DirIcon → theme icon matching the desktop Icon name
/// (largest) → any app icon under usr/share/icons.
fn copy_icon(root: &Path, name: &str, icon_name: Option<&str>) -> Option<String> {
    let src = find_icon_source(root, icon_name)?;
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("png");
    let icons = manifest::icons_dir();
    if std::fs::create_dir_all(&icons).is_err() {
        return None;
    }
    let dest = icons.join(format!("xpm-appimage-{}.{}", name, ext));
    match std::fs::copy(&src, &dest) {
        Ok(_) => Some(dest.to_string_lossy().to_string()),
        Err(e) => {
            warn!("appimage: icon copy failed: {}", e);
            None
        }
    }
}

fn find_icon_source(root: &Path, icon_name: Option<&str>) -> Option<PathBuf> {
    let diricon = root.join(".DirIcon");
    if let Ok(real) = std::fs::canonicalize(&diricon)
        && real.is_file() {
            return Some(real);
        }

    let mut files = Vec::new();
    collect_files(&root.join("usr/share/icons"), &mut files, 0);
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
                files.push(p);
            }
        }
    }
    let is_icon = |p: &Path| {
        matches!(
            p.extension().and_then(|e| e.to_str()),
            Some("png") | Some("svg") | Some("xpm")
        )
    };

    let size_hint = |p: &Path| -> i64 {
        let s = p.to_string_lossy();
        for n in ["512", "256", "192", "128", "96", "64", "48"] {
            if s.contains(n) {
                return n.parse().unwrap_or(0);
            }
        }
        if s.contains("scalable") { 300 } else { 0 }
    };
    let wanted = icon_name.map(|s| s.to_lowercase());

    let mut best: Option<(i64, PathBuf)> = None;
    for p in files.into_iter().filter(|p| is_icon(p)) {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        let name_match = wanted.as_deref().map(|w| stem == w).unwrap_or(false);
        let score = (if name_match { 100_000 } else { 0 }) + size_hint(&p);
        if best.as_ref().map(|(b, _)| score > *b).unwrap_or(true) {
            best = Some((score, p));
        }
    }
    best.map(|(_, p)| p)
}

fn desktop_value(content: &str, key: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(key) {
            let rest = rest.trim_start();
            if let Some(val) = rest.strip_prefix('=') {
                return Some(val.trim().to_string());
            }
        }
    }
    None
}

fn write_desktop(
    appimage: &Path,
    name: &str,
    display_name: &str,
    icon_path: &Option<String>,
    categories: &str,
) -> Option<String> {
    let apps = manifest::applications_dir();
    if std::fs::create_dir_all(&apps).is_err() {
        return None;
    }
    let exec = appimage.to_string_lossy();
    let icon_line = match icon_path {
        Some(p) => p.clone(),
        None => "application-x-executable".to_string(),
    };
    let content = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name={display_name}\n\
         Exec=\"{exec}\" %U\n\
         TryExec={exec}\n\
         Icon={icon}\n\
         Categories={categories}\n\
         Terminal=false\n\
         X-XPM-AppImage=true\n",
        display_name = display_name,
        exec = exec,
        icon = icon_line,
        categories = categories,
    );
    let dest = apps.join(format!("xpm-appimage-{}.desktop", name));
    if std::fs::write(&dest, content).is_ok() {
        Some(dest.to_string_lossy().to_string())
    } else {
        None
    }
}

fn refresh_desktop_db() {
    let apps = manifest::applications_dir();
    let _ = Command::new("update-desktop-database")
        .arg(&apps)
        .output();
}
