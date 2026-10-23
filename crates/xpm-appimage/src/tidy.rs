//! Keeps the AppImage store tidy: version-free names and no leftovers from updates.

use crate::manifest::{self, AppImageEntry};
use std::path::{Path, PathBuf};

fn rename_file(from: &Path, to: &Path) -> bool {
    from == to || std::fs::rename(from, to).is_ok()
}

/// Rename a versioned install (`photocraft-0.7.0-linux-x86_64`) to its stable id
/// (`photocraft`): binary, launcher and icon. Entries whose new id is taken are left alone.
fn migrate_entry(entries: &mut [AppImageEntry], index: usize, apps_dir: &Path) -> bool {
    let old = entries[index].name.clone();
    let new = manifest::app_slug(&old);
    if new == old || entries.iter().any(|e| e.name == new) {
        return false;
    }
    let old_path = PathBuf::from(&entries[index].path);
    let Some(dir) = old_path.parent() else { return false };
    let canonical = dir.join(format!("{new}.AppImage"));
    let new_path = if old_path.is_file() {
        if canonical.exists() || !rename_file(&old_path, &canonical) {
            return false;
        }
        canonical
    } else {
        // The binary was already renamed by hand: adopt it if it sits where we expect it.
        match [canonical, dir.join(format!("{new}.appimage"))].into_iter().find(|p| p.is_file()) {
            Some(found) => found,
            None => return false,
        }
    };

    let mut icon_path = entries[index].icon_path.clone();
    if let Some(old_icon) = icon_path.as_deref().map(PathBuf::from).filter(|p| p.is_file()) {
        let ext = old_icon.extension().and_then(|e| e.to_str()).unwrap_or("png");
        let new_icon = old_icon.with_file_name(format!("xpm-appimage-{new}.{ext}"));
        if rename_file(&old_icon, &new_icon) {
            icon_path = Some(new_icon.to_string_lossy().into_owned());
        }
    }

    let mut desktop_path = entries[index].desktop_path.clone();
    if let Some(old_desktop) = desktop_path.as_deref().map(PathBuf::from).filter(|p| p.is_file())
        && let Ok(text) = std::fs::read_to_string(&old_desktop)
    {
        let mut text = text.replace(old_path.to_string_lossy().as_ref(), new_path.to_string_lossy().as_ref());
        if let (Some(from), Some(to)) = (&entries[index].icon_path, &icon_path) {
            text = text.replace(from.as_str(), to.as_str());
        }
        let new_desktop = apps_dir.join(format!("xpm-appimage-{new}.desktop"));
        if std::fs::write(&new_desktop, text).is_ok() {
            if new_desktop != old_desktop {
                let _ = std::fs::remove_file(&old_desktop);
            }
            desktop_path = Some(new_desktop.to_string_lossy().into_owned());
        }
    }

    let entry = &mut entries[index];
    entry.name = new;
    entry.path = new_path.to_string_lossy().into_owned();
    entry.icon_path = icon_path;
    entry.desktop_path = desktop_path;
    true
}

/// `appimageupdatetool --overwrite` keeps the previous version next to the new one as `*.zs-old`.
pub fn remove_update_backup(appimage: &Path) {
    let _ = std::fs::remove_file(format!("{}.zs-old", appimage.display()));
}

fn remove_update_backups(store: &Path) {
    let Ok(entries) = std::fs::read_dir(store) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("zs-old") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Launchers xPM wrote whose AppImage no longer exists (and that the manifest does not track).
fn prune_orphan_launchers(apps_dir: &Path, entries: &[AppImageEntry]) {
    let Ok(dir) = std::fs::read_dir(apps_dir) else { return };
    for file in dir.flatten() {
        let path = file.path();
        let name = file.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("xpm-appimage-") && name.ends_with(".desktop")) {
            continue;
        }
        if entries.iter().any(|e| e.desktop_path.as_deref().map(Path::new) == Some(path.as_path())) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let target = text
            .lines()
            .find_map(|l| l.strip_prefix("TryExec="))
            .map(|t| PathBuf::from(t.trim()));
        if target.is_some_and(|t| !t.exists()) {
            let icon = text.lines().find_map(|l| l.strip_prefix("Icon=")).map(PathBuf::from);
            let _ = std::fs::remove_file(&path);
            if let Some(icon) = icon.filter(|i| {
                i.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("xpm-appimage-"))
            }) {
                let _ = std::fs::remove_file(icon);
            }
        }
    }
}

/// Migrate versioned names and delete update leftovers. Cheap and safe to run repeatedly.
pub fn tidy_store() {
    let apps_dir = manifest::applications_dir();
    let mut entries = manifest::load();
    let mut changed = false;
    for index in 0..entries.len() {
        changed |= migrate_entry(&mut entries, index, &apps_dir);
    }
    if changed {
        let _ = manifest::save(&entries);
    }
    remove_update_backups(&manifest::store_dir());
    for entry in &entries {
        if let Some(dir) = Path::new(&entry.path).parent() {
            remove_update_backups(dir);
        }
    }
    prune_orphan_launchers(&apps_dir, &entries);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xpm-tidy-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str, dir: &Path, apps: &Path) -> AppImageEntry {
        let path = dir.join(format!("{name}.appimage"));
        let icon = dir.join(format!("xpm-appimage-{name}.png"));
        let desktop = apps.join(format!("xpm-appimage-{name}.desktop"));
        std::fs::write(&path, "bin").unwrap();
        std::fs::write(&icon, "png").unwrap();
        std::fs::write(
            &desktop,
            format!("[Desktop Entry]\nExec=\"{}\" %U\nTryExec={}\nIcon={}\n", path.display(), path.display(), icon.display()),
        )
        .unwrap();
        AppImageEntry {
            name: name.to_string(),
            display_name: "App".into(),
            version: "1".into(),
            path: path.to_string_lossy().into_owned(),
            source_url: None,
            github: None,
            update_info: None,
            supports_update: true,
            icon_path: Some(icon.to_string_lossy().into_owned()),
            desktop_path: Some(desktop.to_string_lossy().into_owned()),
            size: 3,
        }
    }

    #[test]
    fn versioned_entry_is_renamed_everywhere() {
        let dir = temp("migrate");
        let apps = dir.join("apps");
        std::fs::create_dir_all(&apps).unwrap();
        let mut entries = vec![entry("photocraft-0.7.0-linux-x86_64", &dir, &apps)];
        assert!(migrate_entry(&mut entries, 0, &apps));
        let e = &entries[0];
        assert_eq!(e.name, "photocraft");
        assert!(e.path.ends_with("photocraft.AppImage") && Path::new(&e.path).is_file());
        assert!(e.icon_path.as_deref().unwrap().ends_with("xpm-appimage-photocraft.png"));
        let desktop = std::fs::read_to_string(e.desktop_path.as_deref().unwrap()).unwrap();
        assert!(desktop.contains("photocraft.AppImage") && desktop.contains("xpm-appimage-photocraft.png"));
        assert!(!desktop.contains("0.7.0"));
        assert!(!apps.join("xpm-appimage-photocraft-0.7.0-linux-x86_64.desktop").exists());
    }

    #[test]
    fn already_renamed_binary_is_adopted() {
        let dir = temp("adopt");
        let apps = dir.join("apps");
        std::fs::create_dir_all(&apps).unwrap();
        let mut entries = vec![entry("photocraft-0.7.0-linux-x86_64", &dir, &apps)];
        std::fs::rename(&entries[0].path, dir.join("photocraft.appimage")).unwrap();
        assert!(migrate_entry(&mut entries, 0, &apps));
        assert_eq!(entries[0].name, "photocraft");
        assert!(entries[0].path.ends_with("photocraft.appimage"));
        let desktop = std::fs::read_to_string(entries[0].desktop_path.as_deref().unwrap()).unwrap();
        assert!(desktop.contains("photocraft.appimage") && !desktop.contains("0.7.0"));
    }

    #[test]
    fn migration_never_overwrites_an_existing_id() {
        let dir = temp("collide");
        let apps = dir.join("apps");
        std::fs::create_dir_all(&apps).unwrap();
        let mut entries = vec![entry("photocraft", &dir, &apps), entry("photocraft-0.7.0", &dir, &apps)];
        assert!(!migrate_entry(&mut entries, 1, &apps));
        assert_eq!(entries[1].name, "photocraft-0.7.0");
    }

    #[test]
    fn update_backups_are_removed_but_apps_stay() {
        let dir = temp("backups");
        let app = dir.join("photocraft.AppImage");
        std::fs::write(&app, "new").unwrap();
        std::fs::write(dir.join("photocraft.AppImage.zs-old"), "old").unwrap();
        remove_update_backup(&app);
        assert!(app.exists() && !dir.join("photocraft.AppImage.zs-old").exists());
        std::fs::write(dir.join("other.AppImage.zs-old"), "old").unwrap();
        remove_update_backups(&dir);
        assert!(!dir.join("other.AppImage.zs-old").exists() && app.exists());
    }

    #[test]
    fn orphan_launchers_are_pruned_only_when_the_binary_is_gone() {
        let dir = temp("orphans");
        let apps = dir.join("apps");
        std::fs::create_dir_all(&apps).unwrap();
        let live = entry("live", &dir, &apps);
        let gone = apps.join("xpm-appimage-gone.desktop");
        std::fs::write(&gone, "[Desktop Entry]\nTryExec=/nonexistent/gone.AppImage\n").unwrap();
        let kept = apps.join("xpm-appimage-unmanaged.desktop");
        std::fs::write(&kept, format!("[Desktop Entry]\nTryExec={}\n", dir.join("live.appimage").display())).unwrap();
        prune_orphan_launchers(&apps, std::slice::from_ref(&live));
        assert!(!gone.exists());
        assert!(kept.exists());
        assert!(Path::new(live.desktop_path.as_deref().unwrap()).exists());
    }
}
