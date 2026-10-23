use crate::elf;
use crate::integration;
use crate::manifest::{self, AppImageEntry};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};
use xpm_core::{
    error::{Error, Result},
    operation::{Operation, OperationKind, OperationResult},
    package::{Package, PackageBackend, PackageInfo, PackageStatus, SearchResult, UpdateInfo, Version},
    source::{PackageSource, ProgressCallback},
};

/// Log sink for an in-progress operation. Lines are streamed to the UI terminal.
/// Called synchronously on the operation's own thread, so no Send/Sync bound.
pub type LogFn<'a> = dyn Fn(&str) + 'a;

pub struct AppImageBackend {
    /// Where new AppImages are downloaded/copied to. Defaults to the store dir.
    /// The manifest, icons and desktop entries always live in standard XDG dirs,
    /// and each manifest entry records an absolute path, so changing this is safe
    /// and existing installs keep working wherever they live.
    install_dir: std::path::PathBuf,
}

impl AppImageBackend {
    pub fn new() -> Result<Self> {
        Self::with_dir(None)
    }

    /// Construct with a custom install directory (None => default store dir).
    pub fn with_dir(dir: Option<std::path::PathBuf>) -> Result<Self> {
        let install_dir = dir
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(manifest::store_dir);
        let _ = std::fs::create_dir_all(manifest::store_dir());
        let _ = std::fs::create_dir_all(&install_dir);
        Ok(Self { install_dir })
    }

    /// Synchronous manifest read for callers that aren't on an async runtime.
    pub fn list_entries(&self) -> Vec<AppImageEntry> {
        manifest::load()
    }

    fn is_url(source: &str) -> bool {
        source.starts_with("http://") || source.starts_with("https://")
    }

    /// Install from a local path or an http(s) URL. Streams progress via `log`.
    pub fn install(&self, source: &str, log: &LogFn<'_>) -> Result<AppImageEntry> {
        let store = self.install_dir.clone();
        std::fs::create_dir_all(&store)?;

        let file_name = if Self::is_url(source) {
            source.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("download.AppImage")
        } else {
            Path::new(source).file_name().and_then(|f| f.to_str()).unwrap_or("app.AppImage")
        };
        let name = manifest::app_slug(file_name);
        let dest = store.join(format!("{name}.AppImage"));

        let (staged, source_url): (PathBuf, Option<String>) = if Self::is_url(source) {
            log(&format!("Downloading {}\n", source));
            download(source, &dest, log)?;
            (dest, Some(source.to_string()))
        } else {
            let src = Path::new(source);
            if !src.exists() {
                return Err(Error::Other(format!("File not found: {}", source)));
            }
            log(&format!("Copying {}\n", source));
            std::fs::copy(src, &dest)?;
            (dest, Some(source.to_string()))
        };

        chmod_exec(&staged)?;

        log("Checking update support…\n");
        let update_info = elf::read_upd_info(&staged);
        let supports_update = update_info.is_some();
        if supports_update {
            log("Update support: yes (embedded update info found)\n");
        } else {
            log("Update support: no (no embedded update info)\n");
        }

        log("Integrating into application menu…\n");
        let integ = integration::integrate(&staged, &name, &name);

        let size = std::fs::metadata(&staged).map(|m| m.len()).unwrap_or(0);
        let entry = AppImageEntry {
            name: name.clone(),
            display_name: integ.display_name,
            version: integ.version.unwrap_or_else(|| "unknown".to_string()),
            path: staged.to_string_lossy().to_string(),
            source_url,
            github: None,
            update_info,
            supports_update,
            icon_path: integ.icon_path,
            desktop_path: integ.desktop_path,
            size,
        };

        let mut entries = manifest::load();
        entries.retain(|e| e.name != entry.name);
        entries.push(entry.clone());
        manifest::save(&entries).map_err(Error::IoError)?;

        if !fuse_available() {
            log("Note: FUSE not detected. Running AppImages needs 'fuse2' (or 'fuse3'); install it if the app won't launch.\n");
        }
        log(&format!("Installed {}\n", entry.display_name));
        Ok(entry)
    }

    /// Resolve a catalog GitHub repo to its latest .AppImage and install it.
    pub fn install_from_github(&self, github: &str, log: &LogFn<'_>) -> Result<AppImageEntry> {
        log(&format!("Resolving latest release for {}…\n", github));
        let url = crate::catalog::resolve_download(github)?;
        let mut entry = self.install(&url, log)?;
        entry.github = Some(github.to_string());
        entry.supports_update = true;
        let mut entries = manifest::load();
        entries.retain(|e| e.name != entry.name);
        entries.push(entry.clone());
        manifest::save(&entries).map_err(Error::IoError)?;
        Ok(entry)
    }

    /// Remove a tracked AppImage by id: delete binary, icon, desktop entry, manifest row.
    pub fn remove_app(&self, name: &str, log: &LogFn<'_>) -> Result<()> {
        let mut entries = manifest::load();
        let Some(pos) = entries.iter().position(|e| e.name == name) else {
            return Err(Error::PackageNotFound(name.to_string()));
        };
        let entry = entries.remove(pos);

        log(&format!("Removing {}\n", entry.display_name));
        if let Err(e) = std::fs::remove_file(&entry.path) {
            warn!("appimage: failed to remove binary {}: {}", entry.path, e);
        }
        crate::tidy::remove_update_backup(Path::new(&entry.path));
        integration::deintegrate(&entry.icon_path, &entry.desktop_path);
        manifest::save(&entries).map_err(Error::IoError)?;
        log("Removed.\n");
        Ok(())
    }

    /// Update a tracked AppImage. Strategies, in order of preference:
    ///   1. recorded GitHub repo - re-resolve the latest release and download it.
    ///      Preferred because it's reliable; `appimageupdatetool` is known to abort
    ///      (uncaught C++ exception) on some update-info transports.
    ///   2. embedded update info + `appimageupdatetool` (efficient binary delta) -
    ///      a soft fallback: any non-success, including a crash signal, falls through.
    ///   3. recorded http(s) source URL - re-download in place.
    pub fn update_app(&self, name: &str, log: &LogFn<'_>) -> Result<AppImageEntry> {
        let entries = manifest::load();
        let Some(entry) = entries.iter().find(|e| e.name == name).cloned() else {
            return Err(Error::PackageNotFound(name.to_string()));
        };
        if !entry.supports_update {
            return Err(Error::Other(format!(
                "{} does not support updating",
                entry.display_name
            )));
        }

        let path = PathBuf::from(&entry.path);
        let mut new_source = entry.source_url.clone();
        let mut done = false;

        if let Some(gh) = &entry.github {
            log(&format!("Resolving latest release for {}…\n", gh));
            match crate::catalog::resolve_download(gh) {
                Ok(url) => {
                    log("Downloading latest release…\n");
                    download(&url, &path, log)?;
                    chmod_exec(&path)?;
                    new_source = Some(url);
                    done = true;
                }
                Err(e) => log(&format!(
                    "GitHub resolve failed ({}); trying other methods…\n",
                    e
                )),
            }
        }

        if !done && entry.update_info.is_some()
          && let Some(tool) = xpm_core::resolve_tool("appimageupdatetool") {
            log("Updating via appimageupdatetool…\n");
            match Command::new(&tool)
                .arg("--overwrite")
                .arg(&path)
                .status()
            {
                Ok(s) if s.success() => {
                    chmod_exec(&path)?;
                    done = true;
                }
                Ok(_) => log("appimageupdatetool could not update this AppImage; trying source URL…\n"),
                Err(e) => log(&format!(
                    "appimageupdatetool unavailable ({}); trying source URL…\n",
                    e
                )),
            }
          }

        if !done
            && let Some(url) = entry.source_url.as_deref().filter(|u| Self::is_url(u)) {
                log("Re-downloading from source URL…\n");
                download(url, &path, log)?;
                chmod_exec(&path)?;
                done = true;
            }

        if !done {
            return Err(Error::Other(
                "Could not update this AppImage (no working update source).".to_string(),
            ));
        }

        crate::tidy::remove_update_backup(&path);
        let integ = integration::integrate(&path, &entry.name, &entry.display_name);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(entry.size);
        let updated = AppImageEntry {
            display_name: integ.display_name,
            version: integ.version.unwrap_or(entry.version.clone()),
            source_url: new_source,
            update_info: elf::read_upd_info(&path),
            supports_update: true,
            icon_path: integ.icon_path.or(entry.icon_path.clone()),
            desktop_path: integ.desktop_path.or(entry.desktop_path.clone()),
            size,
            ..entry
        };
        let mut entries = manifest::load();
        entries.retain(|e| e.name != updated.name);
        entries.push(updated.clone());
        manifest::save(&entries).map_err(Error::IoError)?;
        log(&format!("Updated {}\n", updated.display_name));
        Ok(updated)
    }

    /// Check whether a newer version is available for one entry, without
    /// downloading. GitHub apps compare the freshly-resolved release URL against
    /// the stored one (AM's "URL as version" technique); embedded-update-info apps
    /// defer to `appimageupdatetool --check-for-update` (exit code 1 = available).
    /// Returns `Ok(false)` for anything we can't check.
    pub fn check_update(&self, entry: &AppImageEntry) -> Result<bool> {
        if let Some(gh) = &entry.github {
            let latest = crate::catalog::resolve_download(gh)?;
            return Ok(Some(latest.as_str()) != entry.source_url.as_deref());
        }
        if let Some(tool) = entry
            .update_info
            .is_some()
            .then(|| xpm_core::resolve_tool("appimageupdatetool"))
            .flatten()
        {
            let status = Command::new(&tool)
                .arg("--check-for-update")
                .arg(&entry.path)
                .status()
                .map_err(|e| Error::Other(format!("appimageupdatetool failed: {}", e)))?;
            return Ok(status.code() == Some(1));
        }
        Ok(false)
    }

    /// Scan every installed AppImage and return the ids of those with a pending
    /// update. Network-bound (one GitHub query per catalog app) - run off the UI
    /// thread. Entries that error during the check are skipped, not failed.
    pub fn check_all_updates(&self) -> Vec<String> {
        self.list_entries()
            .iter()
            .filter(|e| self.check_update(e).unwrap_or(false))
            .map(|e| e.name.clone())
            .collect()
    }

    /// Re-fetch the app from its recorded source (GitHub latest, URL, or local
    /// file) and replace it in place. Used for AppImages that lack embedded
    /// update info - the only way to "update" them is to reinstall.
    pub fn reinstall_app(&self, name: &str, log: &LogFn<'_>) -> Result<AppImageEntry> {
        let entries = manifest::load();
        let Some(entry) = entries.iter().find(|e| e.name == name).cloned() else {
            return Err(Error::PackageNotFound(name.to_string()));
        };
        let path = PathBuf::from(&entry.path);
        log(&format!("Reinstalling {}…\n", entry.display_name));

        if let Some(gh) = &entry.github {
            log(&format!("Resolving latest release for {}…\n", gh));
            let url = crate::catalog::resolve_download(gh)?;
            download(&url, &path, log)?;
        } else if let Some(src) = &entry.source_url {
            if Self::is_url(src) {
                log("Re-downloading from source URL…\n");
                download(src, &path, log)?;
            } else {
                let p = Path::new(src);
                if !p.exists() {
                    return Err(Error::Other(format!(
                        "Original file no longer available: {}",
                        src
                    )));
                }
                log(&format!("Re-copying from {}…\n", src));
                std::fs::copy(p, &path)?;
            }
        } else {
            return Err(Error::Other("No source recorded to reinstall from".to_string()));
        }
        chmod_exec(&path)?;

        let update_info = elf::read_upd_info(&path);
        let supports_update = update_info.is_some();
        let integ = integration::integrate(&path, &entry.name, &entry.display_name);
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(entry.size);
        let updated = AppImageEntry {
            display_name: integ.display_name,
            version: integ.version.unwrap_or(entry.version.clone()),
            update_info,
            supports_update,
            icon_path: integ.icon_path.or(entry.icon_path.clone()),
            desktop_path: integ.desktop_path.or(entry.desktop_path.clone()),
            size,
            ..entry
        };
        let mut entries = manifest::load();
        entries.retain(|e| e.name != updated.name);
        entries.push(updated.clone());
        manifest::save(&entries).map_err(Error::IoError)?;
        log(&format!("Reinstalled {}\n", updated.display_name));
        Ok(updated)
    }

    fn entry_to_package(entry: &AppImageEntry) -> Package {
        Package::new(
            entry.name.clone(),
            Version::new(&entry.version),
            entry.display_name.clone(),
            PackageBackend::AppImage,
            PackageStatus::Installed,
            "appimage",
        )
    }
}

#[async_trait]
impl PackageSource for AppImageBackend {
    fn source_id(&self) -> &str {
        "appimage"
    }

    fn display_name(&self) -> &str {
        "AppImage"
    }

    async fn is_available(&self) -> bool {
        true
    }

    async fn search(&self, _query: &str) -> Result<Vec<SearchResult>> {
        Ok(Vec::new())
    }

    async fn list_installed(&self) -> Result<Vec<Package>> {
        tokio::task::spawn_blocking(|| {
            manifest::load().iter().map(AppImageBackend::entry_to_package).collect()
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))
    }

    async fn list_updates(&self) -> Result<Vec<UpdateInfo>> {
        Ok(Vec::new())
    }

    async fn get_package_info(&self, name: &str) -> Result<PackageInfo> {
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let entries = manifest::load();
            let entry = entries
                .iter()
                .find(|e| e.name == name)
                .ok_or_else(|| Error::PackageNotFound(name.clone()))?;
            Ok(PackageInfo {
                package: AppImageBackend::entry_to_package(entry),
                url: entry.source_url.clone(),
                licenses: Vec::new(),
                groups: Vec::new(),
                depends: Vec::new(),
                optdepends: Vec::new(),
                provides: Vec::new(),
                conflicts: Vec::new(),
                replaces: Vec::new(),
                installed_size: entry.size,
                download_size: 0,
                build_date: None,
                install_date: None,
                packager: None,
                arch: String::new(),
                reason: None,
            })
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?
    }

    async fn execute(&self, operation: Operation) -> Result<OperationResult> {
        self.execute_with_progress(operation, Box::new(|_| {})).await
    }

    async fn execute_with_progress(
        &self,
        operation: Operation,
        _progress: ProgressCallback,
    ) -> Result<OperationResult> {
        let start = std::time::Instant::now();
        info!("AppImage operation: {:?}", operation.kind);
        let target = operation.packages.first().cloned().unwrap_or_default();

        let backend = AppImageBackend::with_dir(Some(self.install_dir.clone()))?;
        let kind = operation.kind.clone();
        let res = tokio::task::spawn_blocking(move || {
            let noop: &LogFn = &|_: &str| {};
            match kind {
                OperationKind::Install => backend.install(&target, noop).map(|_| ()),
                OperationKind::Remove | OperationKind::RemoveWithDeps => {
                    backend.remove_app(&target, noop)
                }
                OperationKind::Update => backend.update_app(&target, noop).map(|_| ()),
                _ => Ok(()),
            }
        })
        .await
        .map_err(|e| Error::Other(e.to_string()))?;

        let dur = start.elapsed().as_millis() as u64;
        Ok(match res {
            Ok(()) => OperationResult::success(operation, Vec::new(), dur),
            Err(e) => OperationResult::failure(operation, e.to_string(), dur),
        })
    }

    async fn sync_databases(&self) -> Result<()> {
        Ok(())
    }

    async fn get_cache_size(&self) -> Result<u64> {
        Ok(0)
    }

    async fn clean_cache(&self, _keep_versions: usize) -> Result<u64> {
        Ok(0)
    }

    async fn list_orphans(&self) -> Result<Vec<Package>> {
        Ok(Vec::new())
    }
}

fn chmod_exec(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// True if a FUSE runtime is present (needed to *run* type-2 AppImages).
fn fuse_available() -> bool {
    if which("fusermount").is_some() || which("fusermount3").is_some() {
        return true;
    }
    let libs = [
        "/usr/lib/libfuse.so.2",
        "/usr/lib/libfuse3.so.3",
        "/usr/lib/x86_64-linux-gnu/libfuse.so.2",
        "/usr/lib/x86_64-linux-gnu/libfuse3.so.3",
    ];
    libs.iter().any(|p| Path::new(p).exists())
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(cmd))
        .find(|p| p.is_file())
}

/// Download `url` to `dest` using curl, streaming curl's stderr progress to `log`.
/// Written to a `.part` file first so a failed download never destroys an installed app.
fn download(url: &str, dest: &Path, log: &LogFn<'_>) -> Result<()> {
    if which("curl").is_none() {
        return Err(Error::Other("curl is required to download AppImages".to_string()));
    }
    let part = PathBuf::from(format!("{}.part", dest.display()));
    let status = Command::new("curl")
        .args(["-fL", "--retry", "2", "-o"])
        .arg(&part)
        .arg(url)
        .status()
        .map_err(|e| Error::NetworkError(e.to_string()))?;
    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(Error::NetworkError(format!("Download failed: {}", url)));
    }
    std::fs::rename(&part, dest)?;
    log("Download complete.\n");
    Ok(())
}
