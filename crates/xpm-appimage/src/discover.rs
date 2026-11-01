//! Finds AppImages that live outside the xPM store and tracks them like installed ones.

use crate::elf;
use crate::integration::desktop_value;
use crate::manifest::{self, AppImageEntry};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

const SCAN_DEPTH: u32 = 2;

#[derive(Default)]
struct Hint {
    name: Option<String>,
    icon: Option<PathBuf>,
    desktop: Option<PathBuf>,
}

fn has_appimage_extension(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("appimage"))
}

/// An ELF whose header carries the AppImage marker ("AI" + type 1 or 2 at offset 8).
fn has_appimage_magic(path: &Path) -> bool {
    let mut head = [0u8; 11];
    File::open(path).and_then(|mut f| f.read_exact(&mut head)).is_ok()
        && head[..4] == *b"\x7fELF"
        && &head[8..10] == b"AI"
        && matches!(head[10], 1 | 2)
}

fn is_appimage(path: &Path) -> bool {
    path.is_file() && (has_appimage_extension(path) || has_appimage_magic(path))
}

fn exec_tokens(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            ' ' if !quoted => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

/// The AppImage a launcher starts, looking at the first few words of `TryExec`/`Exec`.
fn launcher_target(content: &str) -> Option<PathBuf> {
    ["TryExec", "Exec"]
        .iter()
        .filter_map(|key| desktop_value(content, key))
        .flat_map(|value| exec_tokens(&value).into_iter().take(4))
        .filter(|t| t.starts_with('/'))
        .map(PathBuf::from)
        .find(|p| is_appimage(p))
}

fn scan_launchers(dirs: &[PathBuf], found: &mut BTreeMap<PathBuf, Hint>) {
    for dir in dirs {
        let Ok(read) = std::fs::read_dir(dir) else { continue };
        for file in read.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else { continue };
            if desktop_value(&content, "X-XPM-AppImage").is_some() {
                continue;
            }
            let Some(target) = launcher_target(&content).and_then(|t| std::fs::canonicalize(t).ok()) else { continue };
            let hint = found.entry(target).or_default();
            hint.name = hint.name.take().or_else(|| desktop_value(&content, "Name").filter(|n| !n.is_empty()));
            hint.icon = hint.icon.take().or_else(|| {
                desktop_value(&content, "Icon").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_file())
            });
            hint.desktop = hint.desktop.take().or(Some(path));
        }
    }
}

fn scan_dir(dir: &Path, depth: u32, found: &mut BTreeMap<PathBuf, Hint>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for file in read.flatten() {
        let path = file.path();
        if file.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        match file.file_type() {
            Ok(t) if t.is_dir() => {
                if depth < SCAN_DEPTH {
                    scan_dir(&path, depth + 1, found);
                }
            }
            _ if has_appimage_extension(&path) && path.is_file() => {
                if let Ok(real) = std::fs::canonicalize(&path) {
                    found.entry(real).or_default();
                }
            }
            _ => {}
        }
    }
}

fn version_from_file_name(stem: &str) -> Option<String> {
    stem.split(['-', '_'])
        .skip(1)
        .find(|t| t.trim_start_matches(['v', 'V']).starts_with(|c: char| c.is_ascii_digit()) && t.contains('.'))
        .map(|t| t.trim_start_matches(['v', 'V']).to_string())
}

fn unique_name(base: &str, entries: &[AppImageEntry]) -> String {
    let mut name = base.to_string();
    let mut n = 2;
    while entries.iter().any(|e| e.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    name
}

fn adopt(path: &Path, hint: Hint, user_apps: &Path, entries: &[AppImageEntry]) -> AppImageEntry {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("app.AppImage");
    let stem = file_name.trim_end_matches(".AppImage").trim_end_matches(".appimage");
    let update_info = elf::read_upd_info(path);
    AppImageEntry {
        name: unique_name(&manifest::app_slug(file_name), entries),
        display_name: hint.name.unwrap_or_else(|| manifest::app_slug(file_name)),
        version: version_from_file_name(stem).unwrap_or_else(|| "unknown".to_string()),
        path: path.to_string_lossy().into_owned(),
        source_url: None,
        github: None,
        supports_update: update_info.is_some(),
        update_info,
        icon_path: hint.icon.map(|p| p.to_string_lossy().into_owned()),
        desktop_path: hint.desktop.filter(|d| d.starts_with(user_apps)).map(|d| d.to_string_lossy().into_owned()),
        size: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        external: true,
    }
}

/// Drops tracked outside AppImages that were deleted, then adds the ones found in `launcher_dirs` and `scan_dirs`.
fn sync_entries(
    entries: &mut Vec<AppImageEntry>,
    launcher_dirs: &[PathBuf],
    scan_dirs: &[PathBuf],
    user_apps: &Path,
) -> bool {
    let before = entries.len();
    entries.retain(|e| !e.external || Path::new(&e.path).is_file());
    let mut changed = entries.len() != before;

    let mut found = BTreeMap::new();
    scan_launchers(launcher_dirs, &mut found);
    for dir in scan_dirs {
        scan_dir(dir, 0, &mut found);
    }
    let store = std::fs::canonicalize(manifest::store_dir()).unwrap_or_else(|_| manifest::store_dir());
    for (path, hint) in found {
        let tracked = entries
            .iter()
            .any(|e| std::fs::canonicalize(&e.path).map(|p| p == path).unwrap_or(false));
        if tracked || path.starts_with(&store) {
            continue;
        }
        let entry = adopt(&path, hint, user_apps, entries);
        entries.push(entry);
        changed = true;
    }
    changed
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
}

/// Adds AppImages found on the system (menu launchers, common app folders) to the manifest and
/// forgets deleted ones. Cheap enough to run on every list.
pub fn sync() {
    let user_apps = manifest::applications_dir();
    let mut launcher_dirs = vec![user_apps.clone()];
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_default();
    let data_dirs = if data_dirs.is_empty() { "/usr/local/share:/usr/share".to_string() } else { data_dirs };
    launcher_dirs.extend(data_dirs.split(':').filter(|d| !d.is_empty()).map(|d| Path::new(d).join("applications")));

    let home = home();
    let scan_dirs: Vec<PathBuf> = ["Applications", "AppImages", "Apps", ".local/bin", "bin"]
        .iter()
        .map(|d| home.join(d))
        .chain(["/opt", "/usr/local/bin"].iter().map(PathBuf::from))
        .collect();

    let mut entries = manifest::load();
    if sync_entries(&mut entries, &launcher_dirs, &scan_dirs, &user_apps) {
        let _ = manifest::save(&entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xpm-discover-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_appimage(path: &Path) {
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[8..11].copy_from_slice(b"AI\x02");
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn folder_scan_adopts_appimages_once() {
        let root = temp("scan");
        let apps = root.join("Applications");
        std::fs::create_dir_all(apps.join("nested")).unwrap();
        std::fs::write(apps.join("Cool-App-1.2.3-x86_64.AppImage"), "x").unwrap();
        std::fs::write(apps.join("nested/other.appimage"), "x").unwrap();
        std::fs::write(apps.join("readme.txt"), "x").unwrap();

        let mut entries = Vec::new();
        assert!(sync_entries(&mut entries, &[], std::slice::from_ref(&apps), &root));
        let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Cool-App", "other"]);
        assert!(entries.iter().all(|e| e.external));
        assert_eq!(entries.iter().find(|e| e.name == "Cool-App").unwrap().version, "1.2.3");
        assert!(!sync_entries(&mut entries, &[], std::slice::from_ref(&apps), &root), "second pass finds nothing new");
    }

    #[test]
    fn launcher_points_at_an_appimage_anywhere() {
        let root = temp("launcher");
        let elsewhere = root.join("tools/odd place");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let binary = elsewhere.join("thing");
        fake_appimage(&binary);
        let menu = root.join("applications");
        std::fs::create_dir_all(&menu).unwrap();
        std::fs::write(
            menu.join("thing.desktop"),
            format!("[Desktop Entry]\nName=Thing\nExec=\"{}\" %U\nIcon=thing\n", binary.display()),
        )
        .unwrap();
        std::fs::write(menu.join("firefox.desktop"), "[Desktop Entry]\nName=Firefox\nExec=/usr/bin/true\n").unwrap();
        std::fs::write(
            menu.join("ours.desktop"),
            format!("[Desktop Entry]\nExec={}\nX-XPM-AppImage=true\n", binary.display()),
        )
        .unwrap();

        let mut found = BTreeMap::new();
        scan_launchers(std::slice::from_ref(&menu), &mut found);
        assert_eq!(found.len(), 1);
        let hint = found.values().next().unwrap();
        assert_eq!(hint.name.as_deref(), Some("Thing"));
        assert!(hint.desktop.is_some());
    }

    #[test]
    fn deleted_outside_appimages_are_forgotten() {
        let root = temp("gone");
        let apps = root.join("AppImages");
        std::fs::create_dir_all(&apps).unwrap();
        let file = apps.join("temp-app.AppImage");
        std::fs::write(&file, "x").unwrap();
        let mut entries = Vec::new();
        sync_entries(&mut entries, &[], std::slice::from_ref(&apps), &root);
        assert_eq!(entries.len(), 1);
        std::fs::remove_file(&file).unwrap();
        assert!(sync_entries(&mut entries, &[], std::slice::from_ref(&apps), &root));
        assert!(entries.is_empty());
    }

    #[test]
    fn colliding_names_get_a_suffix() {
        let root = temp("collide");
        let a = root.join("a");
        let b = root.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("same.AppImage"), "1").unwrap();
        std::fs::write(b.join("same.AppImage"), "2").unwrap();
        let mut entries = Vec::new();
        sync_entries(&mut entries, &[], &[a, b], &root);
        let mut names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["same", "same-2"]);
    }
}
