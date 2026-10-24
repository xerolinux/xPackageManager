
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use slint::{Model, ModelRc, SharedString, VecModel, Timer, TimerMode};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use serde_json::Value;
use std::rc::Rc;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;
use xpm_alpm::AlpmBackend;
use xpm_alpm::history as alpmhist;
use xpm_core::source::PackageSource;
use xpm_flatpak::FlatpakBackend;
use xpm_flatpak::permissions as fperm;
use xpm_appimage::{AppImageBackend, AppImageEntry, CatalogEntry};

mod appstream;
mod flathub;
mod icons;
mod progress;
mod xml;
use appstream::CachedRemoteApp;
use progress::{Mode, OpReporter};
use xml::XmlEvents;

slint::include_modules!();

fn instance_path(ext: &str) -> String {
    let uid = unsafe { libc::getuid() };
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    format!("{dir}/xpackagemanager-{uid}.{ext}")
}

fn instance_lock_path() -> String {
    instance_path("lock")
}

fn instance_socket_path() -> String {
    instance_path("sock")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlatpakFile {
    Install,
    Repo,
}

fn flatpak_file_kind(path: &str) -> Option<FlatpakFile> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".flatpak") || lower.ends_with(".flatpakref") {
        Some(FlatpakFile::Install)
    } else if lower.ends_with(".flatpakrepo") {
        Some(FlatpakFile::Repo)
    } else {
        None
    }
}

fn is_chaotic_aur_enabled() -> bool {
    std::fs::read_to_string("/etc/pacman.conf")
        .map(|s| s.lines().any(|l| l.trim() == "[chaotic-aur]"))
        .unwrap_or(false)
}

fn acquire_instance_lock() -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(instance_lock_path())
        .ok()?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 { Some(file) } else { None }
}

fn signal_existing_instance(file: Option<&str>) {
    use std::io::Write;
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(instance_socket_path()) {
        let _ = write!(stream, "show\n{}", file.unwrap_or(""));
    }
}

fn notify_updates_once(window: &MainWindow, notified: &std::cell::Cell<bool>) {
    let total = window.get_total_update_count();
    if notified.get() || total <= 0 || !window.get_setting_notify_on_updates() {
        return;
    }
    notified.set(true);
    thread::spawn(move || {
        let msg = format!("{} update{} available", total, if total == 1 { "" } else { "s" });
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=xpm", "--icon=system-software-update", "xPackageManager", &msg])
            .status();
    });
}

fn activate_window(window: &MainWindow) {
    use i_slint_core::window::WindowInner;
    let adapter = WindowInner::from_pub(window.window()).window_adapter();
    if let Some(internal) = adapter.internal(i_slint_core::InternalToken) {
        let _ = internal.bring_to_front();
    }
}

fn listen_for_instance_signals(window: slint::Weak<MainWindow>) {
    use std::io::Read;
    let path = instance_socket_path();
    let _ = std::fs::remove_file(&path);
    if let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) {
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut msg = String::new();
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let _ = (&mut stream).take(4096).read_to_string(&mut msg);
                let file = msg.lines().nth(1).map(str::to_string)
                    .filter(|p| Path::new(p).is_absolute() && flatpak_file_kind(p).is_some() && Path::new(p).is_file());
                let win = window.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(w) = win.upgrade() {
                        w.window().set_minimized(false);
                        w.show().ok();
                        activate_window(&w);
                        if let Some(path) = file {
                            w.invoke_open_flatpak_file(SharedString::from(path.as_str()));
                        }
                    }
                }).ok();
            }
        });
    }
}


struct FlatpakDetail {
    app_id: String,
    name: String,
    summary: String,
    description: String,
    developer: String,
    version: String,
    version_date: String,
    license: String,
    url_homepage: String,
    url_bugtracker: String,
    url_translate: String,
    url_vcs: String,
    url_donation: String,
    url_help: String,
    categories: Vec<String>,
    releases: Vec<(String, String, String)>,
    captions: Vec<String>,
}

type ConflictCtx = Arc<Mutex<Option<(String, Vec<String>, i32)>>>;

enum UiMessage {
    PackagesLoaded {
        installed: Vec<PackageData>,
        updates: Vec<PackageData>,
        flatpak_updates: Vec<PackageData>,
        flatpak: Vec<PackageData>,
        stats: StatsData,
    },
    SearchResults(Vec<PackageData>),
    SetLoading(bool),
    SetBusy(bool),
    SetStatus(String),
    SetProgress(i32),
    SetProgressText(String),
    SetTerminalIsUpgrade(bool),
    ShowProgressPopup(String),
    OperationProgress(i32, String),
    ProgressOutput(String),
    ProgressPrompt(String),
    ProgressHidePrompt,
    ProgressPromptButtons,
    ProgressErrorSummary(String),
    ProgressAutoExpand,
    ProgressMode { cancellable: bool },
    ProgressStage(String),
    OperationDone(bool),
    ActivityLoaded(Vec<ActivityItem>),
    SysInfoLoaded(SysInfo),
    ShowConflict { summary: String, can_force: bool },
    FlatpakRemotesLoaded(Vec<String>),
    RemoteAppsFiltered { serial: u64, apps: Vec<PackageData>, total_matches: usize },
    FlatpakDetailReady(Box<FlatpakDetail>),
    FlatpakScreenshotReady { app_id: String, index: usize, path: String },
    FlatpakExtrasReady { app_id: String, extras: flathub::Extras },
    FlatpakIconReady(String),
    FlatpakAddonsReady(Vec<PackageData>),
    PacmanReposLoaded(Vec<String>),
    RepoPackagesLoaded(Vec<PackageData>),
    RepoPkgDetail(String),
    InstalledFlatpaksLoaded(Vec<PackageData>),
    DepTreeLoaded { deps: Vec<DepNode>, reqby: Vec<DepNode>, root_version: String },
    ArchNewsLoaded(Vec<ArchNewsItem>),
    ArchNewsLoading,
    ShowWarning { message: String, chaotic_aur: bool },
    RepoListLoaded(Vec<(String, bool, String)>),
    PacmanOptsLoaded(PacmanOpts),
    UpdateCacheSize(String),
    PkgInfoLoaded(String),
    InstalledAppImagesLoaded(Vec<AppImageEntry>),
    AppImageUpdatesChecked(Vec<String>),
    AppImageUpdateCleared(String),
    AppImageCatalogReady,
    AppImageCatalogLoading(bool),
    AppImageIconReady { github: String, path: String },
    AppImageIconPixels { github: String, pixels: icons::Pixels },
    AppImageCardsRefresh,
    IconIndexReady(HashMap<String, std::path::PathBuf>),
    IconDecoded { name: String, pixels: Option<icons::Pixels> },
}

#[derive(Clone)]
struct PacmanOpts {
    color: bool,
    love_candy: bool,
    verbose_pkg_lists: bool,
    disable_dl_timeout: bool,
    check_space: bool,
    disable_sandbox: bool,
    no_progress_bar: bool,
    use_syslog: bool,
    clean_method: i32,
}

const FAKE_FZF_SCRIPT: &str = r#"#!/usr/bin/env bash
lines=()
while IFS= read -r line; do lines+=("$line"); done
printf "\n" >/dev/tty
for ((i=${#lines[@]}-1; i>=0; i--)); do
    printf "%s\n" "${lines[$i]}" >/dev/tty
done
printf "\nEnter number to select (0 to cancel): " >/dev/tty
# Disable PTY echo - the UI handles local echo itself to avoid duplicates
stty -echo </dev/tty 2>/dev/null
read -r num </dev/tty
stty echo </dev/tty 2>/dev/null
[[ -z "$num" || "$num" == "0" ]] && exit 1
for line in "${lines[@]}"; do
    if [[ "$line" =~ (^|[^0-9])"$num"\) ]]; then
        printf "%s\n" "$line"
        exit 0
    fi
done
exit 1
"#;


fn parse_pacman_repos(content: &str) -> Vec<(String, bool, String)> {
    let mut repos: Vec<(String, bool, String)> = Vec::new();
    let mut current: Option<(String, bool, String)> = None;

    for line in content.lines() {
        let trimmed = line.trim();
        let uncommented = trimmed.trim_start_matches('#').trim();

        if uncommented.starts_with('[') && uncommented.ends_with(']') {
            if let Some(r) = current.take()
                && r.0 != "options" {
                    repos.push(r);
                }
            let name = uncommented[1..uncommented.len() - 1].to_string();
            let enabled = !trimmed.starts_with('#');
            current = Some((name, enabled, String::new()));
            continue;
        }

        if let Some(ref mut cur) = current
            && cur.0 != "options" && cur.2.is_empty() {
                let effective = if trimmed.starts_with('#') {
                    trimmed.trim_start_matches('#').trim()
                } else {
                    trimmed
                };
                if effective.starts_with("Include") || effective.starts_with("Server") {
                    cur.2 = effective.to_string();
                }
            }
    }
    if let Some(r) = current
        && r.0 != "options" {
            repos.push(r);
        }
    repos
}

fn toggle_repo_in_conf(content: &str, repo_name: &str, enable: bool) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut in_target = false;

    for line in content.lines() {
        let trimmed = line.trim();
        let uncommented = trimmed.trim_start_matches('#').trim();

        if uncommented.starts_with('[') && uncommented.ends_with(']') {
            let name = &uncommented[1..uncommented.len() - 1];
            in_target = name == repo_name;
        }

        if in_target && !trimmed.is_empty() {
            if enable {
                if trimmed.starts_with('#') {
                    result.push(trimmed.strip_prefix('#').unwrap_or(trimmed).trim_start().to_string());
                } else {
                    result.push(line.to_string());
                }
            } else if !trimmed.starts_with('#') {
                result.push(format!("#{}", trimmed));
            } else {
                result.push(line.to_string());
            }
        } else {
            result.push(line.to_string());
        }
    }
    result.join("\n")
}

fn remove_repo_from_conf(content: &str, repo_name: &str) -> String {
    let mut result: Vec<String> = Vec::new();
    let mut in_target = false;

    for line in content.lines() {
        let trimmed = line.trim();
        let uncommented = trimmed.trim_start_matches('#').trim();

        if uncommented.starts_with('[') && uncommented.ends_with(']') {
            let name = &uncommented[1..uncommented.len() - 1];
            in_target = name == repo_name;
        } else if trimmed.is_empty() {
            in_target = false;
        }

        if !in_target {
            result.push(line.to_string());
        }
    }

    let mut collapsed: Vec<String> = Vec::new();
    let mut prev_blank = false;
    for line in result {
        let is_blank = line.trim().is_empty();
        if is_blank && prev_blank {
            continue;
        }
        prev_blank = is_blank;
        collapsed.push(line);
    }
    collapsed.join("\n")
}

fn add_repo_to_conf(content: &str, name: &str, server: &str, siglevel: &str) -> String {
    let mut result = content.to_string();
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result.push('\n');
    result.push_str(&format!("[{}]\n", name));
    if !siglevel.is_empty() {
        result.push_str(&format!("SigLevel = {}\n", siglevel));
    }
    if server.starts_with('/') {
        result.push_str(&format!("Include = {}\n", server));
    } else {
        result.push_str(&format!("Server = {}\n", server));
    }
    result
}

fn write_pacman_conf(content: &str) -> bool {
    let tmp = format!(
        "/tmp/xpm_pacman_conf_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    if std::fs::write(&tmp, content).is_err() {
        return false;
    }
    let script = format!("cp '{}' /etc/pacman.conf && rm -f '{}'", tmp, tmp);
    let ok = std::process::Command::new("pkexec")
        .args(["bash", "-c", &script])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let _ = std::fs::remove_file(&tmp);
    ok
}

fn parse_pacman_opts(content: &str) -> PacmanOpts {
    let bool_keys = [
        "color", "ilovecandy", "verbosepkglists", "disabledownloadtimeout",
        "checkspace", "disablesandbox", "noprogressbar", "usesyslog",
    ];
    let mut in_opts = false;
    let mut flags = [false; 8];
    let mut clean_method = 0i32;

    for line in content.lines() {
        let t = line.trim();
        if t == "[options]" { in_opts = true; continue; }
        if in_opts && t.starts_with('[') { break; }
        if !in_opts || t.starts_with('#') { continue; }
        let tl = t.to_lowercase();
        for (i, key) in bool_keys.iter().enumerate() {
            if tl == *key { flags[i] = true; }
        }
        if tl.starts_with("cleanmethod") && tl.contains("keepcurrent") {
            clean_method = 1;
        }
    }
    PacmanOpts {
        color: flags[0],
        love_candy: flags[1],
        verbose_pkg_lists: flags[2],
        disable_dl_timeout: flags[3],
        check_space: flags[4],
        disable_sandbox: flags[5],
        no_progress_bar: flags[6],
        use_syslog: flags[7],
        clean_method,
    }
}

fn write_pacman_opts(content: &str, opts: &PacmanOpts) -> String {
    let bool_opts: &[(&str, bool)] = &[
        ("Color", opts.color),
        ("ILoveCandy", opts.love_candy),
        ("VerbosePkgLists", opts.verbose_pkg_lists),
        ("DisableDownloadTimeout", opts.disable_dl_timeout),
        ("CheckSpace", opts.check_space),
        ("DisableSandbox", opts.disable_sandbox),
        ("NoProgressBar", opts.no_progress_bar),
        ("UseSyslog", opts.use_syslog),
    ];
    let mut in_opts = false;
    let mut result: Vec<String> = Vec::new();
    let mut seen = [false; 8];
    let mut seen_clean = false;

    for line in content.lines() {
        let t = line.trim();
        if t == "[options]" {
            in_opts = true;
            result.push(line.to_string());
            continue;
        }
        if in_opts && t.starts_with('[') {
            for (i, (kw, en)) in bool_opts.iter().enumerate() {
                if !seen[i] && *en { result.push(kw.to_string()); }
            }
            if !seen_clean && opts.clean_method == 1 {
                result.push("CleanMethod = KeepCurrent".to_string());
            }
            in_opts = false;
            result.push(line.to_string());
            continue;
        }
        if !in_opts { result.push(line.to_string()); continue; }

        let stripped = t.trim_start_matches('#').trim().to_lowercase();
        let mut handled = false;
        for (i, (kw, en)) in bool_opts.iter().enumerate() {
            if stripped == kw.to_lowercase() {
                seen[i] = true;
                if *en { result.push(kw.to_string()); } else { result.push(format!("#{}", kw)); }
                handled = true;
                break;
            }
        }
        if !handled {
            if stripped.starts_with("cleanmethod") {
                seen_clean = true;
                if opts.clean_method == 1 {
                    result.push("CleanMethod = KeepCurrent".to_string());
                } else {
                    result.push("#CleanMethod = KeepInstalled".to_string());
                }
            } else {
                result.push(line.to_string());
            }
        }
    }
    if in_opts {
        for (i, (kw, en)) in bool_opts.iter().enumerate() {
            if !seen[i] && *en { result.push(kw.to_string()); }
        }
        if !seen_clean && opts.clean_method == 1 {
            result.push("CleanMethod = KeepCurrent".to_string());
        }
    }
    result.join("\n")
}

const CONFLICT_PATTERNS: &[&str] = &[
    "conflicting files",
"are in conflict",
"exists in filesystem",
"breaks dependency",
"could not satisfy dependencies",
"failed to commit transaction",
];


#[derive(Serialize, Deserialize, Clone)]
struct AppConfig {
    flatpak_enabled: bool,
    check_updates_on_start: bool,
    #[serde(default = "default_parallel_downloads")]
    parallel_downloads: u32,
    #[serde(default)]
    aur_pill_dismissed: bool,
    #[serde(default)]
    distro_warning_dismissed: bool,
    #[serde(default = "default_font_scale")]
    font_scale: f32,
    #[serde(default)]
    notify_on_updates: bool,
    #[serde(default)]
    auto_clean_cache: bool,
    #[serde(default)]
    appimage_enabled: bool,
    #[serde(default)]
    appimage_dir: String,
    #[serde(default)]
    appimage_feeds: Vec<AppImageFeed>,
    #[serde(default)]
    appimage_github_token: String,
    #[serde(default)]
    flatpak_remotes: Vec<FlatpakRemoteCfg>,
    #[serde(default)]
    history_warn_dismissed: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct AppImageFeed {
    name: String,
    url: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct FlatpakRemoteCfg {
    name: String,
    url: String,
}

fn default_appimage_feeds() -> Vec<AppImageFeed> {
    vec![AppImageFeed {
        name: "AppImageHub".to_string(),
        url: xpm_appimage::catalog::FEED_URL.to_string(),
    }]
}

fn default_parallel_downloads() -> u32 { 5 }
fn default_font_scale() -> f32 { 1.0 }

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            flatpak_enabled: true,
            check_updates_on_start: false,
            parallel_downloads: 5,
            aur_pill_dismissed: false,
            distro_warning_dismissed: false,
            font_scale: 1.0,
            notify_on_updates: false,
            auto_clean_cache: false,
            appimage_enabled: false,
            appimage_dir: String::new(),
            appimage_feeds: default_appimage_feeds(),
            appimage_github_token: String::new(),
            flatpak_remotes: Vec::new(),
            history_warn_dismissed: false,
        }
    }
}


fn config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(format!("{}/.config/xpm/config.json", home))
}

fn load_config() -> AppConfig {
    let path = config_path();
    if path.exists()
        && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(config) = serde_json::from_str::<AppConfig>(&content) {
                return config;
            }
    AppConfig::default()
}

fn save_config(config: &AppConfig) {
    let path = config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(&path, json);
    }
}

fn build_config(window: &MainWindow) -> AppConfig {
    AppConfig {
        flatpak_enabled: window.get_setting_flatpak_enabled(),
        check_updates_on_start: window.get_setting_check_updates_on_start(),
        parallel_downloads: window.get_setting_parallel_downloads() as u32,
        aur_pill_dismissed: window.get_aur_pill_dismissed(),
        distro_warning_dismissed: window.get_distro_warning_dismissed(),
        font_scale: window.global::<Cat>().get_font_scale(),
        notify_on_updates: window.get_setting_notify_on_updates(),
        auto_clean_cache: window.get_setting_auto_clean_cache(),
        appimage_enabled: window.get_setting_appimage_enabled(),
        appimage_dir: window.get_setting_appimage_dir().to_string(),
        appimage_feeds: window
            .get_appimage_sources()
            .iter()
            .map(|s| AppImageFeed { name: s.name.to_string(), url: s.url.to_string() })
            .collect(),
        appimage_github_token: window.get_setting_appimage_github_token().to_string(),
        // Not UI-editable fields; preserve whatever xpm has recorded.
        flatpak_remotes: load_config().flatpak_remotes,
        history_warn_dismissed: load_config().history_warn_dismissed,
    }
}

/// User-scoped flatpak remotes (name + url) as xpm currently sees them.
fn list_user_flatpak_remotes() -> Vec<FlatpakRemoteCfg> {
    std::process::Command::new("flatpak")
        .args(["--user", "remotes", "--columns=name,url"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty() && l.trim() != "Name")
        .filter_map(|l| {
            let mut parts = l.split('\t');
            let name = parts.next()?.trim().to_string();
            let url = parts.next().unwrap_or("").trim().to_string();
            if name.is_empty() { None } else { Some(FlatpakRemoteCfg { name, url }) }
        })
        .collect()
}

/// Persist the user-scoped flatpak remotes into xpm's config, preserving the
/// rest of the settings.
fn save_xpm_remotes() {
    let mut cfg = load_config();
    cfg.flatpak_remotes = list_user_flatpak_remotes();
    save_config(&cfg);
}

/// If the browse dropdown's selected remote is no longer in `remotes` (removed or
/// disabled), fall back to flathub/first and reload its content.
fn switch_remote_if_gone(window: &MainWindow, _gone: &str, remotes: &[String]) {
    let selected = window.get_selected_remote().to_string();
    if remotes.iter().any(|r| r == &selected) {
        return;
    }
    let fallback = remotes.iter().find(|r| r.as_str() == "flathub").cloned()
        .or_else(|| remotes.first().cloned())
        .unwrap_or_default();
    window.set_selected_remote(SharedString::from(fallback.as_str()));
    window.invoke_browse_remote(SharedString::from(fallback.as_str()));
}

fn read_pacman_parallel_downloads() -> Option<u32> {
    let content = std::fs::read_to_string("/etc/pacman.conf").ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') { continue; }
        if let Some(rest) = trimmed.strip_prefix("ParallelDownloads") {
            let val_str = rest.trim_start_matches([' ', '=']).trim();
            if let Ok(n) = val_str.parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

fn is_arch_package(path: &str) -> bool {
    let extensions = [".pkg.tar.zst", ".pkg.tar.xz", ".pkg.tar.gz", ".pkg.tar"];
    extensions.iter().any(|ext| path.ends_with(ext))
}

fn get_local_package_info(path: &str) -> Option<PackageData> {
    let path_obj = Path::new(path);
    if !path_obj.exists() {
        return None;
    }

    let filename = path_obj.file_name()?.to_str()?;

    let base = filename
    .strip_suffix(".pkg.tar.zst")
    .or_else(|| filename.strip_suffix(".pkg.tar.xz"))
    .or_else(|| filename.strip_suffix(".pkg.tar.gz"))
    .or_else(|| filename.strip_suffix(".pkg.tar"))?;

    let parts: Vec<&str> = base.rsplitn(4, '-').collect();
    let (name, version) = if parts.len() >= 3 {
        let name = parts[3..].join("-");
        let version = format!("{}-{}", parts[2], parts[1]);
        (name, version)
    } else {
        (base.to_string(), "unknown".to_string())
    };

    let size = path_obj
    .metadata()
    .ok()
    .map(|m| format_size(m.len()))
    .unwrap_or_else(|| "Unknown".to_string());

    Some(PackageData {
        name: SharedString::from(&name),
         display_name: SharedString::from(&name),
         version: SharedString::from(&version),
         description: SharedString::from(format!("Local package: {}", filename)),
         repository: SharedString::from("local"),
         backend: 2,
         installed: false,
         has_update: false,
         installed_size: SharedString::from(&size),
         licenses: SharedString::from(""),
         url: SharedString::from(""),
         dependencies: SharedString::from(""),
         required_by: SharedString::from(""),
         selected: false,
         explicit: false,
    })
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}


/// Build a unified-Updates-page row (PackageData, backend=3) for an AppImage that
/// has a pending update. Per-row action routes to `update-appimage`.
fn appimage_entry_to_update_row(entry: &AppImageEntry) -> PackageData {
    let current = if entry.version == "unknown" || entry.version.is_empty() {
        "installed".to_string()
    } else {
        entry.version.clone()
    };
    PackageData {
        name: SharedString::from(entry.name.as_str()),
        display_name: SharedString::from(entry.display_name.as_str()),
        version: SharedString::from("update available"),
        description: SharedString::from(format!("current: {}", current).as_str()),
        repository: SharedString::from("appimage"),
        backend: 3,
        installed: true,
        has_update: true,
        installed_size: SharedString::from(format_size(entry.size).as_str()),
        licenses: SharedString::from(""),
        url: SharedString::from(""),
        dependencies: SharedString::from(""),
        required_by: SharedString::from(""),
        selected: false,
        explicit: false,
    }
}

/// Rows for the Updates page = installed AppImages whose id is in the pending set.
fn build_appimage_update_rows(
    entries: &[AppImageEntry],
    updates: &std::collections::HashSet<String>,
) -> Vec<PackageData> {
    entries
        .iter()
        .filter(|e| updates.contains(&e.name))
        .map(appimage_entry_to_update_row)
        .collect()
}

fn entry_to_installed_card(
    entry: &AppImageEntry,
    updates: &std::collections::HashSet<String>,
) -> AppImageInstalled {
    let (icon, has_icon) = match &entry.icon_path {
        Some(p) if std::path::Path::new(p).exists() => {
            match slint::Image::load_from_path(std::path::Path::new(p)) {
                Ok(img) => (img, true),
                Err(_) => (slint::Image::default(), false),
            }
        }
        _ => (slint::Image::default(), false),
    };
    let size = format_size(entry.size);
    let detail = if entry.version == "unknown" || entry.version.is_empty() {
        size
    } else {
        format!("{}  •  {}", entry.version, size)
    };
    let initial = entry
        .display_name
        .chars()
        .next()
        .map(|c| c.to_uppercase().next().unwrap_or(c).to_string())
        .unwrap_or_default();
    AppImageInstalled {
        id: SharedString::from(entry.name.as_str()),
        name: SharedString::from(entry.display_name.as_str()),
        detail: SharedString::from(detail.as_str()),
        supports_update: entry.supports_update,
        update_available: updates.contains(&entry.name),
        initial: SharedString::from(initial.as_str()),
        icon,
        has_icon,
    }
}


fn appimage_icon_dir() -> std::path::PathBuf {
    let base = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            format!("{}/.cache", home)
        });
    std::path::PathBuf::from(base).join("xpm/appimage-icons")
}

fn icon_cache_path(github: &str, icon_url: &str) -> std::path::PathBuf {
    let slug: String = github
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let ext = icon_url.rsplit('.').next().filter(|e| e.len() <= 4).unwrap_or("png");
    appimage_icon_dir().join(format!("{}.{}", slug, ext))
}

/// Map of installed catalog apps: github repo (lowercase) -> manifest id.
fn installed_github_map(entries: &[AppImageEntry]) -> std::collections::HashMap<String, String> {
    entries
        .iter()
        .filter_map(|e| e.github.as_ref().map(|gh| (gh.to_lowercase(), e.name.clone())))
        .collect()
}

fn set_appimage_card_icon(window: &MainWindow, github: &str, icon: slint::Image) {
    let model = window.get_catalog_appimages();
    for idx in 0..model.row_count() {
        if let Some(mut card) = model.row_data(idx)
            && card.github == github
        {
            card.icon = icon;
            card.has_icon = true;
            model.set_row_data(idx, card);
            break;
        }
    }
}

fn catalog_entry_to_card(
    entry: &CatalogEntry,
    installed: &std::collections::HashMap<String, String>,
) -> AppImageCard {
    let installed_id = installed.get(&entry.github.to_lowercase()).cloned();
    let desc = if entry.description.is_empty() {
        entry.categories.join(", ")
    } else {
        entry.description.clone()
    };
    let initial = entry
        .name
        .chars()
        .next()
        .map(|c| c.to_uppercase().next().unwrap_or(c).to_string())
        .unwrap_or_default();
    let category = entry.categories.first().cloned().unwrap_or_default();

    AppImageCard {
        github: SharedString::from(entry.github.as_str()),
        name: SharedString::from(entry.name.as_str()),
        description: SharedString::from(desc.as_str()),
        initial: SharedString::from(initial.as_str()),
        category: SharedString::from(category.as_str()),
        icon: slint::Image::default(),
        has_icon: false,
        installed: installed_id.is_some(),
        installed_id: SharedString::from(installed_id.unwrap_or_default().as_str()),
    }
}

/// Rows rendered per catalog page. Keeps first paint snappy on the ~1k-app feed.
const APPIMAGE_PER_PAGE: usize = 50;

/// Clamp a 0-based page index to the last valid page for `total` matches.
fn clamp_appimage_page(page: usize, total: usize) -> usize {
    page.min(total.saturating_sub(1) / APPIMAGE_PER_PAGE)
}

/// Rows per page for the Flatpak and Repo store browsers (Prev/Next pagination).
const BROWSE_PAGE_SIZE: usize = 100;

/// Clamp a 0-based page to the last valid page for `total` items at BROWSE_PAGE_SIZE.
fn clamp_browse_page(page: usize, total: usize) -> usize {
    page.min(total.saturating_sub(1) / BROWSE_PAGE_SIZE)
}

/// Filter (and rank) the repo package list by a query against name/description.
fn filter_repo_list(full: &[PackageData], query: &str) -> Vec<PackageData> {
    let q = query.to_lowercase();
    let mut filtered: Vec<PackageData> = if q.is_empty() {
        full.to_vec()
    } else {
        full.iter()
            .filter(|p| {
                p.name.to_lowercase().contains(&q) || p.description.to_lowercase().contains(&q)
            })
            .cloned()
            .collect()
    };
    if !q.is_empty() {
        filtered.sort_by_key(|p| {
            let name = p.name.to_lowercase();
            if name == q {
                0u8
            } else if name.starts_with(&q) {
                1
            } else if name.contains(&q) {
                2
            } else {
                3
            }
        });
    }
    filtered
}

/// Render one page of the repo browser: slice `filtered` at `page` and push the
/// page slice + total + clamped page index into the window.
fn render_repo_page(w: &MainWindow, filtered: &[PackageData], page: usize) {
    let total = filtered.len();
    let page = clamp_browse_page(page, total);
    let slice: Vec<PackageData> = filtered
        .iter()
        .skip(page * BROWSE_PAGE_SIZE)
        .take(BROWSE_PAGE_SIZE)
        .cloned()
        .collect();
    w.set_repo_packages(ModelRc::new(VecModel::from(slice)));
    w.set_repo_total_matches(total as i32);
    w.set_repo_page(page as i32);
}

/// Filter the catalog by a query against name/description, then return the cards
/// for the requested page (0-based) plus the total number of matches. The page is
/// clamped so an out-of-range index lands on the last valid page.
fn filter_catalog(
    catalog: &[CatalogEntry],
    query: &str,
    source: &str,
    installed: &std::collections::HashMap<String, String>,
    page: usize,
) -> (Vec<AppImageCard>, usize) {
    let q = query.trim().to_lowercase();
    // Empty source or "All" means every source.
    let src = source.trim();
    let all_sources = src.is_empty() || src.eq_ignore_ascii_case("All");
    let matches: Vec<&CatalogEntry> = catalog
        .iter()
        .filter(|e| all_sources || e.source == src)
        .filter(|e| {
            q.is_empty()
                || e.name.to_lowercase().contains(&q)
                || e.description.to_lowercase().contains(&q)
        })
        .collect();
    let total = matches.len();
    let last_page = total.saturating_sub(1) / APPIMAGE_PER_PAGE;
    let page = page.min(last_page);
    let cards = matches
        .into_iter()
        .skip(page * APPIMAGE_PER_PAGE)
        .take(APPIMAGE_PER_PAGE)
        .map(|e| catalog_entry_to_card(e, installed))
        .collect();
    (cards, total)
}

/// Run kdialog, or zenity only if kdialog is not installed; a cancelled kdialog must not open a second dialog.
fn run_picker(kdialog_args: &[&str], zenity_args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("kdialog")
        .args(kdialog_args)
        .output()
        .or_else(|_| std::process::Command::new("zenity").args(zenity_args).output())
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!p.is_empty()).then_some(p)
}

fn pick_file(title: &str, kdialog_filter: &str, zenity_filter: &str) -> Option<String> {
    run_picker(
        &["--getopenfilename", ".", kdialog_filter],
        &["--file-selection", &format!("--title={title}"), &format!("--file-filter={zenity_filter}")],
    )
}

fn pick_appimage_file() -> Option<String> {
    pick_file(
        "Select AppImage",
        "AppImage (*.AppImage *.appimage)",
        "AppImage | *.AppImage *.appimage",
    )
}

fn pick_flatpak_file() -> Option<String> {
    pick_file(
        "Select Flatpak file",
        "Flatpak (*.flatpak *.flatpakref *.flatpakrepo)",
        "Flatpak | *.flatpak *.flatpakref *.flatpakrepo",
    )
}

/// Open a native folder chooser. Tries kdialog, falls back to zenity.
fn pick_directory() -> Option<String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_string());
    run_picker(
        &["--getexistingdirectory", &home],
        &["--file-selection", "--directory", "--title=Select AppImage folder"],
    )
}

/// Run an AppImage operation on the calling (background) thread, streaming log
/// output into the progress popup and refreshing the installed list afterward.
fn run_appimage_op<F>(
    tx: &mpsc::Sender<UiMessage>,
    title: &str,
    dir: Option<String>,
    clear_update: Option<String>,
    op: F,
) where
    F: FnOnce(&AppImageBackend, &dyn Fn(&str)) -> xpm_core::error::Result<()>,
{
    let reporter = OpReporter::start(tx, title, Mode { cancellable: false }, 1);
    let feeder = reporter.feeder();

    let backend = match AppImageBackend::with_dir(dir.map(std::path::PathBuf::from)) {
        Ok(b) => b,
        Err(e) => {
            feeder.line(&format!("Error: {e}"));
            reporter.finish(false);
            return;
        }
    };

    let log = |line: &str| {
        feeder.feed(line.as_bytes());
    };
    let ok = match op(&backend, &log) {
        Ok(()) => true,
        Err(e) => {
            feeder.line(&format!("\nError: {e}"));
            false
        }
    };
    if ok && let Some(id) = clear_update {
        let _ = tx.send(UiMessage::AppImageUpdateCleared(id));
    }
    reporter.finish(ok);

    let _ = tx.send(UiMessage::InstalledAppImagesLoaded(backend.list_entries()));
    let _ = tx.send(UiMessage::AppImageCardsRefresh);
}

fn clean_dep_name(s: &str) -> String {
    let s = s.trim();
    for sep in &[">=", "<=", ">", "<", "="] {
        if let Some((name, _)) = s.split_once(sep) {
            return name.trim().to_string();
        }
    }
    s.to_string()
}

/// For a file-path dep (starts with '/'), resolve it to the owning package name
/// using `pacman -Qo <path>`. Returns None if unresolvable or not installed.
fn resolve_file_dep(path: &str) -> Option<String> {
    let out = std::process::Command::new("pacman")
        .args(["-Qo", path])
        .output()
        .ok()?;
    if !out.status.success() { return None; }
    let text = String::from_utf8_lossy(&out.stdout);
    let pkg = text.split("is owned by ").nth(1)?.split_whitespace().next()?.to_string();
    if pkg.is_empty() { None } else { Some(pkg) }
}

/// Resolve raw dep tokens to package names: file paths via `pacman -Qo`, sonames
/// dropped, plain names kept.
fn resolve_dep_list(raw: Vec<String>) -> Vec<String> {
    let mut result = Vec::with_capacity(raw.len());
    for dep in raw {
        if dep.starts_with('/') {
            if let Some(pkg) = resolve_file_dep(&dep)
                && !result.contains(&pkg) {
                    result.push(pkg);
                }
        } else if dep.contains(".so") {
        } else {
            result.push(dep);
        }
    }
    result
}

/// Check if a package is available in repos (installable but not installed).
fn is_installable(pkg_name: &str) -> bool {
    let out = std::process::Command::new("pacman")
        .args(["-Ss", pkg_name])
        .output()
        .ok();
    match out {
        Some(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout);
            text.lines().any(|line| {
                line.contains("/") && line.split_whitespace().next().is_some_and(|name| {
                    name.ends_with(&format!("/{}/", pkg_name)) || name.ends_with(&format!("/{}", pkg_name))
                })
            })
        }
        _ => false,
    }
}

/// Batch check which packages are installable from repos.
/// Returns a set of package names that are available but not installed.
fn batch_installable_check(names: &[&str]) -> HashMap<String, bool> {
    if names.is_empty() { return HashMap::new(); }

    let mut result: HashMap<String, bool> = HashMap::new();

    for name in names {
        result.insert(name.to_string(), is_installable(name));
    }

    result
}

/// Trim VCS/AUR version suffixes for display.
/// "5.2.1+r604+g0b99615a8aef-1" → "5.2.1"
/// "2.43+r5+g856c426a7534-1"   → "2.43"
fn trim_version(v: &str) -> String {
    v.split('+').next().unwrap_or(v).trim_end_matches('-').to_string()
}

/// Run `pacman -Q` once and return HashMap<name, version> for all installed pkgs.
fn all_installed_map() -> HashMap<String, String> {
    let Ok(out) = std::process::Command::new("pacman").arg("-Q").output() else {
        return HashMap::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(2, ' ');
            let name = it.next()?.to_string();
            let ver  = it.next()?.trim().to_string();
            Some((name, ver))
        })
        .collect()
}

/// Parse `pacman -Qi <pkg>` (or multi-pkg) output.
/// Returns (depends, optional_deps, required_by) for the FIRST package block.
fn parse_qi_block(text: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut depends: Vec<String>  = Vec::new();
    let mut optional: Vec<String> = Vec::new();
    let mut reqby: Vec<String>    = Vec::new();
    let mut state = 0u8;

    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            match state {
                1 => depends.extend(tokens.iter().filter(|&&t| t != "None").map(|&t| clean_dep_name(t))),
                2 => optional.extend(tokens.iter().filter(|&&t| t != "None").map(|&t| {
                    clean_dep_name(t.split(':').next().unwrap_or(t))
                })),
                3 => reqby.extend(tokens.iter().filter(|&&t| t != "None").map(|&t| t.to_string())),
                _ => {}
            }
            continue;
        }

        state = 0;
        if let Some(val) = line.strip_prefix("Depends On").and_then(|r| r.split_once(':').map(|x| x.1)) {
            state = 1;
            depends.extend(val.split_whitespace().filter(|&t| t != "None").map(clean_dep_name));
        } else if let Some(val) = line.strip_prefix("Optional Deps").and_then(|r| r.split_once(':').map(|x| x.1)) {
            state = 2;
            optional.extend(val.split_whitespace().filter(|&t| t != "None").map(|t| {
                clean_dep_name(t.split(':').next().unwrap_or(t))
            }));
        } else if let Some(val) = line.strip_prefix("Required By").and_then(|r| r.split_once(':').map(|x| x.1)) {
            state = 3;
            reqby.extend(val.split_whitespace().filter(|&t| t != "None").map(|t| t.to_string()));
        }
    }
    (resolve_dep_list(depends), resolve_dep_list(optional), reqby)
}

/// Batch-query deps for many packages in a single `pacman -Qi` call.
/// Returns HashMap<pkg_name, Vec<dep_name>>.
fn batch_deps(names: &[&str]) -> HashMap<String, Vec<String>> {
    if names.is_empty() { return HashMap::new(); }
    let Ok(out) = std::process::Command::new("pacman")
        .arg("-Qi").args(names).output()
    else { return HashMap::new(); };

    let text = String::from_utf8_lossy(&out.stdout);
    let mut result: HashMap<String, Vec<String>> = HashMap::new();
    let mut cur_name = String::new();
    let mut cur_deps: Vec<String> = Vec::new();
    let mut in_depends = false;

    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if in_depends {
                cur_deps.extend(line.split_whitespace()
                    .filter(|&t| t != "None").map(clean_dep_name));
            }
            continue;
        }
        in_depends = false;

        if let Some(val) = line.strip_prefix("Name").and_then(|r| r.split_once(':').map(|x| x.1)) {
            if !cur_name.is_empty() {
                result.insert(cur_name.clone(), resolve_dep_list(std::mem::take(&mut cur_deps)));
            }
            cur_name = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("Depends On").and_then(|r| r.split_once(':').map(|x| x.1)) {
            in_depends = true;
            cur_deps.extend(val.split_whitespace().filter(|&t| t != "None").map(clean_dep_name));
        }
    }
    if !cur_name.is_empty() {
        result.insert(cur_name, resolve_dep_list(cur_deps));
    }
    result
}

/// Parse `pacman -Si <pkg>` output for a non-installed package.
/// Returns (depends, optional_deps) - no required-by for uninstalled packages.
fn parse_si_block(text: &str) -> (Vec<String>, Vec<String>) {
    let mut depends: Vec<String> = Vec::new();
    let mut optional: Vec<String> = Vec::new();
    let mut state = 0u8;

    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            let val = line.trim();
            match state {
                1 => depends.extend(val.split_whitespace()
                    .filter(|&t| t != "None")
                    .map(|t| clean_dep_name(t.split(':').next().unwrap_or(t)))),
                2 => optional.extend(val.split_whitespace()
                    .filter(|&t| t != "None")
                    .map(|t| clean_dep_name(t.split(':').next().unwrap_or(t)))),
                _ => {}
            }
            continue;
        }
        if let Some(val) = line.strip_prefix("Depends On").and_then(|r| r.split_once(':').map(|x| x.1)) {
            state = 1;
            depends.extend(val.split_whitespace()
                .filter(|&t| t != "None")
                .map(|t| clean_dep_name(t.split(':').next().unwrap_or(t))));
        } else if let Some(val) = line.strip_prefix("Optional Deps").and_then(|r| r.split_once(':').map(|x| x.1)) {
            state = 2;
            optional.extend(val.split_whitespace()
                .filter(|&t| t != "None")
                .map(|t| clean_dep_name(t.split(':').next().unwrap_or(t))));
        } else if line.contains(':') && !line.starts_with(' ') {
            state = 0;
        }
    }
    (resolve_dep_list(depends), resolve_dep_list(optional))
}

/// Build the full dep tree data for `pkg_name`.
/// Returns (dep_nodes, reqby_nodes, root_version).
/// Root is NOT included in dep_nodes - rendered separately as a pill card.
fn build_dep_tree(pkg_name: &str) -> (Vec<DepNode>, Vec<DepNode>, String) {
    let installed = all_installed_map();
    let pkg_installed = installed.contains_key(pkg_name);

    let (direct_deps, opt_deps, reqby_names, root_version) = if pkg_installed {
        let root_version = installed.get(pkg_name).map(|v| trim_version(v)).unwrap_or_default();
        let Ok(root_out) = std::process::Command::new("pacman")
            .args(["-Qi", pkg_name]).output()
        else {
            return (vec![], vec![], root_version);
        };
        let root_text = String::from_utf8_lossy(&root_out.stdout);
        let (d, o, r) = parse_qi_block(&root_text);
        (d, o, r, root_version)
    } else {
        let Ok(root_out) = std::process::Command::new("pacman")
            .args(["-Si", pkg_name]).output()
        else {
            return (vec![], vec![], String::new());
        };
        let root_text = String::from_utf8_lossy(&root_out.stdout);
        let ver = root_text.lines()
            .find(|l| l.starts_with("Version"))
            .and_then(|l| l.split_once(':').map(|x| x.1))
            .map(|v| trim_version(v.trim()))
            .unwrap_or_default();
        let (d, o) = parse_si_block(&root_text);
        (d, o, vec![], ver)
    };

    let all_l1: Vec<String> = direct_deps.iter().chain(opt_deps.iter()).cloned().collect();
    let l1_installed: Vec<&str> = all_l1.iter()
        .filter(|n| installed.contains_key(n.as_str()))
        .map(|n| n.as_str()).collect();
    let l2_map = batch_deps(&l1_installed);

    let mut all_missing: Vec<String> = all_l1.iter()
        .filter(|n| !installed.contains_key(n.as_str()))
        .cloned().collect();
    for sub_list in l2_map.values() {
        for s in sub_list {
            if !installed.contains_key(s.as_str()) {
                all_missing.push(s.clone());
            }
        }
    }
    all_missing.sort_unstable();
    all_missing.dedup();
    let missing_refs: Vec<&str> = all_missing.iter().map(|s| s.as_str()).collect();
    let installable_map = batch_installable_check(&missing_refs);

    let show_dep = |name: &str| -> bool {
        installed.contains_key(name) || *installable_map.get(name).unwrap_or(&false)
    };

    let vis_direct: Vec<&String> = direct_deps.iter().filter(|n| show_dep(n)).collect();
    let vis_opt: Vec<&String>    = opt_deps.iter().filter(|n| show_dep(n)).collect();

    let mut dep_nodes: Vec<DepNode> = Vec::new();

    let n_direct = vis_direct.len();
    let n_opt    = vis_opt.len();

    for (idx, dep_name) in vis_direct.iter().enumerate() {
        let is_last_direct = idx == n_direct - 1;
        let connector = if is_last_direct && n_opt == 0 { "└─ " } else { "├─ " };
        let ver = installed.get(dep_name.as_str()).map(|v| trim_version(v)).unwrap_or_default();
        let is_installed = !ver.is_empty();
        let installable = if !is_installed { *installable_map.get(dep_name.as_str()).unwrap_or(&false) } else { false };

        dep_nodes.push(DepNode {
            name: SharedString::from(dep_name.as_str()),
            version: SharedString::from(&ver),
            depth: 1,
            installed: is_installed,
            is_optional: false,
            prefix: SharedString::from(connector),
            is_root: false,
            installable,
        });

        if let Some(sub_deps) = l2_map.get(dep_name.as_str()) {
            let vis_subs: Vec<&String> = sub_deps.iter().filter(|s| show_dep(s)).collect();
            if vis_subs.is_empty() { continue; }

            let parent_cont = if is_last_direct && n_opt == 0 { "   " } else { "│  " };
            let nsub = vis_subs.len();
            for (j, sub) in vis_subs.iter().enumerate() {
                let sc = if j == nsub - 1 { "└─ " } else { "├─ " };
                let sv = installed.get(sub.as_str()).map(|v| trim_version(v)).unwrap_or_default();
                let sv_installed = !sv.is_empty();
                let sv_installable = if !sv_installed { *installable_map.get(sub.as_str()).unwrap_or(&false) } else { false };
                dep_nodes.push(DepNode {
                    name: SharedString::from(sub.as_str()),
                    version: SharedString::from(&sv),
                    depth: 2,
                    installed: sv_installed,
                    is_optional: false,
                    prefix: SharedString::from(format!("{}{}", parent_cont, sc)),
                    is_root: false,
                    installable: sv_installable,
                });
            }
        }
    }

    if !vis_opt.is_empty() {
        dep_nodes.push(DepNode {
            name: SharedString::from("Optional Dependencies"),
            version: SharedString::from(""),
            depth: -1,
            installed: false,
            is_optional: true,
            prefix: SharedString::from(""),
            is_root: false,
            installable: false,
        });

        for (idx, dep_name) in vis_opt.iter().enumerate() {
            let connector = if idx == n_opt - 1 { "└╌ " } else { "├╌ " };
            let ver = installed.get(dep_name.as_str()).map(|v| trim_version(v)).unwrap_or_default();
            let is_installed = !ver.is_empty();
            let installable = if !is_installed { *installable_map.get(dep_name.as_str()).unwrap_or(&false) } else { false };
            dep_nodes.push(DepNode {
                name: SharedString::from(dep_name.as_str()),
                version: SharedString::from(&ver),
                depth: 1,
                installed: is_installed,
                is_optional: true,
                prefix: SharedString::from(connector),
                is_root: false,
                installable,
            });
        }
    }

    let reqby_nodes: Vec<DepNode> = reqby_names.iter().map(|name| {
        let ver = installed.get(name.as_str()).map(|v| trim_version(v)).unwrap_or_default();
        DepNode {
            name: SharedString::from(name.as_str()),
            version: SharedString::from(&ver),
            depth: 1,
            installed: true,
            is_optional: false,
            prefix: SharedString::from(""),
            is_root: false,
            installable: false,
        }
    }).collect();

    (dep_nodes, reqby_nodes, root_version)
}

fn spawn_in_pty(cmd: &str, args: &[&str]) -> Result<(i32, u32), String> {
    use std::os::unix::io::FromRawFd;

    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;

    let winsize = libc::winsize { ws_col: 80, ws_row: 40, ws_xpixel: 0, ws_ypixel: 0 };
    let ret = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &winsize) };
    if ret != 0 {
        return Err("openpty failed".to_string());
    }

    let child: Result<std::process::Child, std::io::Error> = unsafe {
        let stdin_fd = libc::dup(slave);
        let stdout_fd = libc::dup(slave);
        let stderr_fd = libc::dup(slave);
        std::process::Command::new(cmd)
        .args(args)
        .env("TERM", "xterm-256color")
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .stdin(std::process::Stdio::from_raw_fd(stdin_fd))
        .stdout(std::process::Stdio::from_raw_fd(stdout_fd))
        .stderr(std::process::Stdio::from_raw_fd(stderr_fd))
        .pre_exec(move || {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY, 0);
            Ok(())
        })
        .spawn()
    };

    unsafe { libc::close(slave); }

    match child {
        Ok(c) => Ok((master, c.id())),
        Err(e) => {
            unsafe { libc::close(master); }
            Err(format!("Failed to spawn {}: {}", cmd, e))
        }
    }
}


fn run_in_terminal(
    tx: &mpsc::Sender<UiMessage>,
    title: &str,
    cmd: &str,
    args: &[&str],
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
) {
    let reporter = OpReporter::start(tx, title, Mode { cancellable: true }, 1);
    match progress::run_pty(&reporter.feeder(), cmd, args, input_sender, pid_holder) {
        Ok(success) => reporter.finish(success),
        Err(e) => {
            reporter.feeder().line(&format!("Error: {e}"));
            reporter.finish(false);
        }
    }
}

struct UpdatePlan {
    native: bool,
    flatpak: bool,
    appimages: Vec<String>,
    appimage_dir: String,
}

// Native + system flatpaks share one pkexec (root); user flatpaks run as the invoking
// user, since root alone misses user-install apps (e.g. user-added remotes).
fn update_script(native: bool, flatpak: bool) -> Option<String> {
    const USER_FLATPAKS: &str = "flatpak update --user";
    match (native, flatpak) {
        (true, true) => Some(format!(
            "pkexec bash -c 'pacman -Syu && echo \"\" && echo \"=== System flatpaks ===\" && flatpak update --system' && echo \"\" && echo \"=== User flatpaks ===\" && {USER_FLATPAKS}"
        )),
        (true, false) => Some("pkexec pacman -Syu".to_string()),
        (false, true) => Some(format!(
            "echo \"=== User flatpaks ===\" && {USER_FLATPAKS} && echo \"\" && echo \"=== System flatpaks ===\" && pkexec flatpak update --system"
        )),
        (false, false) => None,
    }
}

/// One popup, one run: every enabled backend with pending updates, in sequence.
fn run_update_all(
    tx: &mpsc::Sender<UiMessage>,
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
    plan: UpdatePlan,
) {
    let reporter = OpReporter::start(tx, "Updating Everything", Mode { cancellable: true }, 1);
    let feeder = reporter.feeder();
    let has_appimages = !plan.appimages.is_empty();
    let mut success = true;

    if let Some(script) = update_script(plan.native, plan.flatpak) {
        if has_appimages {
            feeder.window(0, 75);
        }
        match progress::run_pty(&feeder, "bash", &["-c", &script], input_sender, pid_holder) {
            Ok(ok) => success = ok,
            Err(e) => {
                feeder.line(&format!("Error: {e}"));
                success = false;
            }
        }
    }

    if success && has_appimages && !progress::cancel_requested() {
        let base = if plan.native || plan.flatpak { 75 } else { 0 };
        feeder.line("\n=== AppImages ===");
        match AppImageBackend::with_dir(Some(std::path::PathBuf::from(&plan.appimage_dir))) {
            Ok(backend) => {
                let total = plan.appimages.len() as i32;
                for (i, name) in plan.appimages.iter().enumerate() {
                    if progress::cancel_requested() {
                        feeder.line("Cancelled.");
                        success = false;
                        break;
                    }
                    let i = i as i32;
                    feeder.window(base + (100 - base) * i / total, base + (100 - base) * (i + 1) / total);
                    let _ = tx.send(UiMessage::ProgressStage(format!("Updating {name}...")));
                    feeder.line(&format!("\n- {name} -"));
                    let log = |line: &str| {
                        feeder.feed(line.as_bytes());
                    };
                    match backend.update_app(name, &log) {
                        Ok(_) => {
                            let _ = tx.send(UiMessage::AppImageUpdateCleared(name.clone()));
                        }
                        Err(e) => {
                            feeder.line(&format!("Failed: {e}"));
                            success = false;
                        }
                    }
                }
                let _ = tx.send(UiMessage::InstalledAppImagesLoaded(backend.list_entries()));
                let _ = tx.send(UiMessage::AppImageCardsRefresh);
            }
            Err(e) => {
                feeder.line(&format!("Error: {e}"));
                success = false;
            }
        }
    }

    reporter.finish(success);
}

struct RepoFile {
    name: String,
    title: String,
    url: String,
}

fn parse_flatpakrepo(path: &str) -> Option<RepoFile> {
    let text = std::fs::read_to_string(path).ok()?;
    let field = |key: &str| {
        text.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    let url = field("Url")?;
    let stem = Path::new(path).file_stem()?.to_string_lossy();
    let name: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .collect();
    if name.is_empty() {
        return None;
    }
    let title = field("Title").unwrap_or_else(|| name.clone());
    Some(RepoFile { name, title, url })
}

fn run_flatpak_file(
    window: &slint::Weak<MainWindow>,
    tx: &mpsc::Sender<UiMessage>,
    path: &str,
    input: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid: &Arc<Mutex<Option<u32>>>,
    ctx: &ConflictCtx,
) {
    let file_name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match flatpak_file_kind(path) {
        Some(FlatpakFile::Install) => {
            let title = format!("Installing {file_name}");
            run_managed_operation(tx, &title, "install-file", &[path.to_string()], 1, input, pid, ctx);
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_refresh();
                }
            });
        }
        Some(FlatpakFile::Repo) => {
            let Some(repo) = parse_flatpakrepo(path) else {
                let reporter = OpReporter::start(tx, &format!("Adding {file_name}"), Mode { cancellable: false }, 1);
                reporter.feeder().line("Error: not a valid .flatpakrepo file (missing Url)");
                reporter.finish(false);
                return;
            };
            let title = format!("Adding remote {}", repo.title);
            let names = [repo.name.clone(), path.to_string(), repo.url.clone()];
            run_managed_operation(tx, &title, "add-remote-file", &names, 1, input, pid, ctx);
            let _ = tx.send(UiMessage::FlatpakRemotesLoaded(fetch_flatpak_remotes()));
            save_xpm_remotes();
            let _ = std::fs::remove_file(remote_cache_path(&repo.name));
            let weak = window.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_load_flatpak_remotes();
                }
            });
        }
        None => {}
    }
}

fn build_pacman_command(action: &str, names: &[String], backend: i32) -> (String, Vec<String>) {
    match (action, backend) {
        ("install", 1) | ("bulk-install", 1) => {
            ("flatpak".to_string(), {
                // No --noninteractive (live progress bar) and no -y (flatpak asks first,
                // like pacman does).
                let mut args = vec!["install".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("install-file", 1) => {
            ("flatpak".to_string(), vec!["install".to_string(), names[0].clone()])
        }
        ("add-remote-file", 1) => {
            const ADD_REMOTE: &str = r#"printf 'Add Flatpak remote "%s" (%s)? [Y/n] ' "$1" "$3"; read -r a; case "$a" in ""|y|Y|yes|Yes) flatpak remote-add --user --if-not-exists "$1" "$2" ;; *) echo Cancelled; exit 1 ;; esac"#;
            ("sh".to_string(), vec![
                "-c".to_string(), ADD_REMOTE.to_string(), "sh".to_string(),
                names[0].clone(), names[1].clone(), names[2].clone(),
            ])
        }
        ("remove", 1) | ("bulk-remove", 1) => {
            ("flatpak".to_string(), {
                let mut args = vec!["uninstall".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("update", 1) => {
            ("flatpak".to_string(), {
                let mut args = vec!["update".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("remove", _) | ("bulk-remove", _) => {
            ("pkexec".to_string(), {
                let mut args = vec!["pacman".to_string(), "-R".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("update-all", 1) => {
            ("flatpak".to_string(), vec!["update".to_string()])
        }
        ("update-all", _) => {
            ("pkexec".to_string(), vec!["pacman".to_string(), "-Syu".to_string()])
        }
        ("force-install", _) => {
            ("pkexec".to_string(), {
                let mut args = vec!["pacman".to_string(), "-S".to_string(),
                    "--overwrite".to_string(), "*".to_string(), "--noconfirm".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("force-update-all", _) => {
            ("pkexec".to_string(), vec![
                "pacman".to_string(), "-Syu".to_string(),
                "--overwrite".to_string(), "*".to_string(), "--noconfirm".to_string(),
            ])
        }
        _ => {
            ("pkexec".to_string(), {
                let mut args = vec!["pacman".to_string(), "-S".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_managed_operation(
    tx: &mpsc::Sender<UiMessage>,
    title: &str,
    action: &str,
    names: &[String],
    backend: i32,
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
    conflict_ctx: &ConflictCtx,
) {
    *conflict_ctx.lock().unwrap() = Some((action.to_string(), names.to_vec(), backend));

    let (cmd, args) = build_pacman_command(action, names, backend);
    let args_str: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let mut reporter =
        OpReporter::start(tx, title, Mode { cancellable: true }, names.len());
    match progress::run_pty(&reporter.feeder(), &cmd, &args_str, input_sender, pid_holder) {
        Err(e) => {
            reporter.feeder().line(&format!("Error: {e}"));
            reporter.finish(false);
        }
        Ok(true) => reporter.finish(true),
        Ok(false) => {
            reporter.settle();
            let output = reporter.captured();
            let lower = output.to_lowercase();
            if CONFLICT_PATTERNS.iter().any(|p| lower.contains(&p.to_lowercase())) {
                let (summary, can_force) = parse_conflict_summary(&output);
                let _ = tx.send(UiMessage::ShowConflict { summary, can_force });
            } else {
                reporter.finish(false);
            }
        }
    }
}

/// Flip `installed` on matching rows without rebuilding the full model.
fn flip_installed_in_model(
    model: ModelRc<PackageData>,
    names: &std::collections::HashSet<&str>,
    installed: bool,
) -> ModelRc<PackageData> {
    let updated: Vec<PackageData> = model.iter().map(|mut p| {
        if names.contains(p.name.as_str()) {
            p.installed = installed;
        }
        p
    }).collect();
    ModelRc::new(VecModel::from(updated))
}

fn package_to_ui(pkg: &xpm_core::package::Package, has_update: bool) -> PackageData {
    let backend = match pkg.backend {
        xpm_core::package::PackageBackend::Pacman => 0,
        xpm_core::package::PackageBackend::Flatpak => 1,
        xpm_core::package::PackageBackend::AppImage => 3,
    };

    PackageData {
        name: SharedString::from(pkg.name.as_str()),
        display_name: SharedString::from(pkg.name.as_str()),
        version: SharedString::from(pkg.version.to_string().as_str()),
        description: SharedString::from(pkg.description.as_str()),
        repository: SharedString::from(pkg.repository.as_str()),
        backend,
        installed: matches!(
            pkg.status,
            xpm_core::package::PackageStatus::Installed | xpm_core::package::PackageStatus::Orphan
        ),
        has_update,
        installed_size: SharedString::from(""),
        licenses: SharedString::from(""),
        url: SharedString::from(""),
        dependencies: SharedString::from(""),
        required_by: SharedString::from(""),
        selected: false,
        explicit: pkg.explicit,
    }
}

fn update_to_ui(update: &xpm_core::package::UpdateInfo) -> PackageData {
    let backend = match update.backend {
        xpm_core::package::PackageBackend::Pacman => 0,
        xpm_core::package::PackageBackend::Flatpak => 1,
        xpm_core::package::PackageBackend::AppImage => 3,
    };

    let version_str = format!(
        "{} → {}",
        update.current_version,
                              update.new_version
    );

    PackageData {
        name: SharedString::from(update.name.as_str()),
        display_name: SharedString::from(update.name.as_str()),
        version: SharedString::from(version_str.as_str()),
        description: SharedString::from(update.repository.as_str()),
        repository: SharedString::from(update.repository.as_str()),
        backend,
        installed: true,
        has_update: true,
        installed_size: SharedString::from(format_size(update.download_size).as_str()),
        licenses: SharedString::from(""),
        url: SharedString::from(""),
        dependencies: SharedString::from(""),
        required_by: SharedString::from(""),
        selected: false,
        explicit: false,
    }
}

fn update_selection_in_model(model: &ModelRc<PackageData>, name: &str, backend: i32, selected: bool) {
    let model = model.as_any().downcast_ref::<VecModel<PackageData>>();
    if let Some(vec_model) = model {
        for i in 0..vec_model.row_count() {
            if let Some(mut row) = vec_model.row_data(i)
                && row.name.as_str() == name && row.backend == backend {
                    row.selected = selected;
                    vec_model.set_row_data(i, row);
                    break;
                }
        }
    }
}

fn find_package_installed(window: &MainWindow, name: &str, backend: i32) -> bool {
    let models: Vec<ModelRc<PackageData>> = vec![
        window.get_installed_packages(),
        window.get_update_packages(),
        window.get_search_installed(),
        window.get_search_available(),
        window.get_flatpak_packages(),
    ];
    for model in &models {
        if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<PackageData>>() {
            for i in 0..vec_model.row_count() {
                if let Some(row) = vec_model.row_data(i)
                    && row.name.as_str() == name && row.backend == backend {
                        return row.installed;
                    }
            }
        }
    }
    false
}

/// Returns true if any package in the native update list requires a reboot
/// (kernel, firmware, microcode, systemd, bootloader, glibc)
fn native_updates_need_reboot(window: &MainWindow) -> bool {
    const REBOOT_PATTERNS: &[&str] = &[
        "linux", "linux-zen", "linux-lts", "linux-hardened", "linux-cachyos",
        "linux-firmware", "linux-firmware-whence",
        "intel-ucode", "amd-ucode",
        "systemd", "systemd-libs",
        "glibc",
        "grub", "refind-efi", "efibootmgr", "syslinux",
        "mkinitcpio",
    ];
    let model = window.get_update_packages();
    for i in 0..model.row_count() {
        let pkg = model.row_data(i).unwrap_or_default();
        let name = pkg.name.to_string();
        if REBOOT_PATTERNS.iter().any(|p| &name == p)
            || (name.starts_with("linux-") && !name.starts_with("linux-docs")
                && !name.starts_with("linux-headers"))
        {
            return true;
        }
    }
    false
}

fn update_selection_in_models(window: &MainWindow, name: &str, backend: i32, selected: bool) {
    update_selection_in_model(&window.get_installed_packages(), name, backend, selected);
    update_selection_in_model(&window.get_update_packages(), name, backend, selected);
    update_selection_in_model(&window.get_search_installed(), name, backend, selected);
    update_selection_in_model(&window.get_search_available(), name, backend, selected);
    update_selection_in_model(&window.get_flatpak_packages(), name, backend, selected);
    update_selection_in_model(&window.get_repo_packages(), name, backend, selected);
}

fn parse_conflict_summary(output: &str) -> (String, bool) {
    let mut lines = Vec::new();
    let mut is_file_conflict = false;

    for line in output.lines() {
        let lower = line.to_lowercase();
        if lower.contains("exists in filesystem") || lower.contains("conflicting files") {
            is_file_conflict = true;
        }
        if lower.contains("error:") || lower.contains("warning:")
            || lower.contains("exists in filesystem")
            || lower.contains("are in conflict")
            || lower.contains("breaks dependency")
            || lower.contains("could not satisfy")
            || lower.contains("conflicting files")
            || lower.contains("conflicting dependencies")
        {
            let t = line.trim();
            if !t.is_empty() {
                lines.push(t.to_string());
            }
        }
    }

    let summary = if lines.is_empty() {
        "A conflict was detected. See the operation log for details.".to_string()
    } else {
        lines.join("\n")
    };

    (summary, is_file_conflict)
}


fn load_recent_activity() -> Vec<ActivityItem> {
    let content = match std::fs::read_to_string("/var/log/pacman.log") {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut items: Vec<ActivityItem> = content
        .lines()
        .filter_map(|line| {
            let alpm_pos = line.find("] [ALPM] ")?;
            let rest = &line[alpm_pos + 9..];
            let (action, pkg_part) = if let Some(s) = rest.strip_prefix("installed ") {
                ("installed", s)
            } else if let Some(s) = rest.strip_prefix("removed ") {
                ("removed", s)
            } else {
                let s = rest.strip_prefix("upgraded ")?;
                ("upgraded", s)
            };
            let pkg = pkg_part.split_whitespace().next().unwrap_or("").to_string();
            if pkg.is_empty() { return None; }

            let date = line.strip_prefix('[')
                .and_then(|s| s.find(']').map(|e| &s[..e]))
                .and_then(|s| s.get(..10))
                .unwrap_or("")
                .to_string();

            Some(ActivityItem {
                action: SharedString::from(action),
                package: SharedString::from(pkg.as_str()),
                date: SharedString::from(date.as_str()),
            })
        })
        .collect();
    items.reverse();
    items.truncate(14);
    items
}

fn load_sys_info() -> SysInfo {
    let kernel = std::fs::read_to_string("/proc/version")
        .unwrap_or_default()
        .split_whitespace()
        .nth(2)
        .unwrap_or("unknown")
        .to_string();

    let uptime_secs: u64 = std::fs::read_to_string("/proc/uptime")
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .map(|f| f as u64)
        .unwrap_or(0);
    let uptime = if uptime_secs >= 86400 {
        format!("{}d {}h {}m", uptime_secs / 86400, (uptime_secs % 86400) / 3600, (uptime_secs % 3600) / 60)
    } else if uptime_secs >= 3600 {
        format!("{}h {}m", uptime_secs / 3600, (uptime_secs % 3600) / 60)
    } else {
        format!("{}m", uptime_secs / 60)
    };

    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| {
            s.trim()
             .replace("(R)", "")
             .replace("(TM)", "")
             .replace("  ", " ")
             .trim()
             .to_string()
        })
        .unwrap_or_else(|| "unknown".to_string());

    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mem_total_kb: u64 = meminfo.lines()
        .find(|l| l.starts_with("MemTotal:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mem_avail_kb: u64 = meminfo.lines()
        .find(|l| l.starts_with("MemAvailable:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let used_mb = (mem_total_kb.saturating_sub(mem_avail_kb)) / 1024;
    let total_mb = mem_total_kb / 1024;
    let (ram_used, ram_total) = if total_mb >= 1024 {
        (format!("{:.1}G", used_mb as f64 / 1024.0), format!("{:.1}G", total_mb as f64 / 1024.0))
    } else {
        (format!("{}M", used_mb), format!("{}M", total_mb))
    };

    let gpu = (|| -> Option<String> {
        for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if !s.starts_with("card") || s.contains('-') { continue; }
            let vendor_path = entry.path().join("device/vendor");
            let device_path = entry.path().join("device/device");
            if let (Ok(vendor), Ok(device)) = (
                std::fs::read_to_string(&vendor_path),
                std::fs::read_to_string(&device_path),
            ) {
                let v = vendor.trim().to_lowercase();
                let prefix = if v == "0x10de" { "NVIDIA" }
                    else if v == "0x1002" { "AMD" }
                    else if v == "0x8086" { "Intel" }
                    else { "GPU" };
                let dev = device.trim().to_string();
                let uevent = entry.path().join("device/uevent");
                if let Ok(ue) = std::fs::read_to_string(uevent)
                    && let Some(line) = ue.lines().find(|l| l.starts_with("PCI_ID=")) {
                        let pci_id = line.trim_start_matches("PCI_ID=");
                        return Some(format!("{} ({})", prefix, pci_id));
                    }
                return Some(format!("{} {}", prefix, dev));
            }
        }
        None
    })().unwrap_or_default();

    let (disk_used, disk_total) = (|| -> Option<(String, String)> {
        let out = std::process::Command::new("df")
            .args(["-h", "/"])
            .output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let line = text.lines().nth(1)?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        let total = parts.get(1)?.to_string();
        let used = parts.get(2)?.to_string();
        Some((used, total))
    })().unwrap_or_default();

    let hostname = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();

    let distro = std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("NAME="))
        .map(|l| l.trim_start_matches("NAME=").trim_matches('"').to_string())
        .unwrap_or_default();

    SysInfo {
        kernel: SharedString::from(kernel.as_str()),
        uptime: SharedString::from(uptime.as_str()),
        cpu: SharedString::from(cpu.as_str()),
        ram_used: SharedString::from(ram_used.as_str()),
        ram_total: SharedString::from(ram_total.as_str()),
        gpu: SharedString::from(gpu.as_str()),
        disk_used: SharedString::from(disk_used.as_str()),
        disk_total: SharedString::from(disk_total.as_str()),
        hostname: SharedString::from(hostname.as_str()),
        distro: SharedString::from(distro.as_str()),
    }
}

fn repo_display_order(repo: &str) -> u8 {
    match repo {
        "core" => 0,
        "extra" => 1,
        "multilib" => 2,
        r if r.contains("testing") => 3,
        "" => 8,
        r if r.starts_with("aur") || r == "local" => 9,
        _ => 5,
    }
}

fn repo_to_avatar_category(repo: &str) -> &'static str {
    match repo {
        "core" => "System",
        "extra" => "Development",
        "multilib" => "Network",
        r if r.contains("testing") => "Science",
        r if r.starts_with("aur") || r.is_empty() => "Game",
        _ => "Utility",
    }
}

fn group_installed_by_repo(pkgs: Vec<PackageData>) -> Vec<PackageData> {
    let mut sorted = pkgs;
    sorted.sort_by(|a, b| {
        repo_display_order(a.repository.as_str())
            .cmp(&repo_display_order(b.repository.as_str()))
            .then_with(|| a.name.as_str().to_lowercase().cmp(&b.name.as_str().to_lowercase()))
    });

    let mut result: Vec<PackageData> = Vec::new();
    let mut last_repo = String::new();

    for pkg in sorted {
        let repo = pkg.repository.to_string();
        if repo != last_repo {
            last_repo = repo.clone();
            let label = if repo.is_empty() { "unknown".to_string() } else { repo.clone() };
            result.push(PackageData {
                name: SharedString::from(label.as_str()),
                display_name: SharedString::from(""),
                version: SharedString::from(""),
                description: SharedString::from(""),
                repository: SharedString::from(repo.as_str()),
                backend: -1,
                installed: false,
                has_update: false,
                installed_size: SharedString::from(""),
                licenses: SharedString::from(""),
                url: SharedString::from(""),
                dependencies: SharedString::from(""),
                required_by: SharedString::from(""),
                selected: false,
                explicit: false,
            });
        }
        let initial = pkg.name.as_str()
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .to_string();
        let category = repo_to_avatar_category(pkg.repository.as_str());
        let mut aug = pkg;
        aug.required_by = SharedString::from(initial.as_str());
        aug.installed_size = SharedString::from(category);
        result.push(aug);
    }

    result
}

fn load_installed_flatpaks() -> Vec<PackageData> {
    let output = std::process::Command::new("flatpak")
        .args(["list", "--app", "--columns=application,name,version,branch"])
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let text = String::from_utf8_lossy(&output.stdout);
    let mut pkgs = Vec::new();

    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 { continue; }
        let app_id = cols[0].trim();
        let display = cols.get(1).copied().unwrap_or(app_id).trim();
        let version = cols.get(2).copied().unwrap_or("").trim();
        if app_id.is_empty() { continue; }
        let initial = display.chars().next().unwrap_or('?').to_uppercase().to_string();
        pkgs.push(PackageData {
            name: SharedString::from(app_id),
            display_name: SharedString::from(display),
            version: SharedString::from(version),
            description: SharedString::from(""),
            repository: SharedString::from("flathub"),
            backend: 1,
            installed: true,
            has_update: false,
            installed_size: SharedString::from(""),
            licenses: SharedString::from(""),
            url: SharedString::from(""),
            dependencies: SharedString::from(""),
            required_by: SharedString::from(initial.as_str()),
            selected: false,
            explicit: false,
        });
    }

    pkgs
}

/// Parse /etc/pacman.conf Include lines and build rate-mirrors commands for
/// each unique mirrorlist file found. Determines the rate-mirrors backend from
/// the filename (e.g. "chaotic" → chaotic-aur target, otherwise → arch).
fn build_mirrorlist_update_script() -> String {
    let content = std::fs::read_to_string("/etc/pacman.conf").unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    let mut cmds: Vec<String> = Vec::new();

    for line in content.lines() {
        let t = line.trim();
        if t.starts_with('#') { continue; }
        if let Some(rest) = t.strip_prefix("Include") {
            let path = rest.trim_start_matches(|c: char| c == '=' || c.is_whitespace()).to_string();
            if path.is_empty() || !seen.insert(path.clone()) { continue; }

            let target = if path.to_lowercase().contains("chaotic") {
                "chaotic-aur"
            } else {
                "arch"
            };
            cmds.push(format!(
                "rate-mirrors --allow-root --protocol https {} | tee {}",
                target, path
            ));
        }
    }

    if cmds.is_empty() {
        "rate-mirrors --allow-root --protocol https arch | tee /etc/pacman.d/mirrorlist".to_string()
    } else {
        cmds.join(" && ")
    }
}

fn is_xerolinux() -> bool {
    std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .any(|l| {
            let l = l.trim();
            (l.starts_with("ID=") || l.starts_with("NAME="))
                && l.to_lowercase().contains("xero")
        })
}

fn fetch_arch_news() -> Vec<ArchNewsItem> {
    let out = match std::process::Command::new("curl")
        .args(["-s", "--max-time", "10", "https://archlinux.org/feeds/news/"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let xml = String::from_utf8_lossy(&out.stdout);
    parse_arch_rss(&xml)
}

fn parse_arch_rss(xml: &str) -> Vec<ArchNewsItem> {
    let mut events = XmlEvents::new(xml.as_bytes());

    let mut items: Vec<ArchNewsItem> = Vec::new();
    let mut in_item = false;
    let mut cur_tag = String::new();
    let mut title = String::new();
    let mut date = String::new();
    let mut link = String::new();
    let mut description = String::new();

    loop {
        match events.next() {
            Event::Start(e) => {
                let tag = e.name().as_ref().to_string();
                if tag == "item" {
                    in_item = true;
                    title.clear(); date.clear(); link.clear(); description.clear();
                }
                cur_tag = tag;
            }
            Event::Text(e) => {
                if !in_item { continue; }
                let text = e.xml_content(quick_xml::XmlVersion::Implicit1_0).into_owned();
                match cur_tag.as_str() {
                    "title" => title = text,
                    "pubDate" => {
                        let parts: Vec<&str> = text.splitn(6, ' ').collect();
                        date = if parts.len() >= 4 {
                            format!("{} {} {}", parts[1], parts[2], parts[3])
                        } else {
                            text
                        };
                    }
                    "link" => link = text,
                    "description" => description = strip_html(&text),
                    _ => {}
                }
            }
            Event::End(e) => {
                let name_bytes = e.name();
                let tag = name_bytes.as_ref();
                if tag == "item" && in_item {
                    in_item = false;
                    let summary = if description.chars().count() > 400 {
                        let cut: String = description.chars().take(400).collect();
                        format!("{}…", cut.trim_end())
                    } else {
                        description.trim().to_string()
                    };
                    items.push(ArchNewsItem {
                        title: SharedString::from(title.trim()),
                        date: SharedString::from(date.trim()),
                        link: SharedString::from(link.trim()),
                        summary: SharedString::from(summary),
                    });
                }
                cur_tag.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    items
}

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---- Flatpak permission editor ----

const PERM_SHARES: &[(&str, &str)] = &[("network", "Network"), ("ipc", "Inter-process communication")];
const PERM_SOCKETS: &[(&str, &str)] = &[
    ("wayland", "Wayland"), ("fallback-x11", "Fallback to X11"), ("x11", "X11 windowing"),
    ("pulseaudio", "PulseAudio"), ("session-bus", "D-Bus session bus"), ("system-bus", "D-Bus system bus"),
    ("ssh-auth", "SSH agent"), ("pcsc", "Smart cards"), ("cups", "Printing"), ("gpg-agent", "GPG agent"),
];
const PERM_DEVICES: &[(&str, &str)] = &[
    ("dri", "GPU acceleration"), ("input", "Input devices"), ("usb", "USB devices"),
    ("kvm", "Virtualization (KVM)"), ("shm", "Shared memory"), ("all", "All devices"),
];
const PERM_FEATURES: &[(&str, &str)] = &[
    ("devel", "Development syscalls"), ("multiarch", "Other architectures"),
    ("bluetooth", "Bluetooth"), ("canbus", "CAN bus"), ("per-app-dev-shm", "Per-app shared memory"),
];
const PERM_FS_FIXED: &[(&str, &str)] = &[
    ("host", "All system files"), ("host-os", "All OS files"), ("host-etc", "All system config (/etc)"),
    ("home", "All user files (home)"), ("xdg-download", "Downloads"), ("xdg-documents", "Documents"),
    ("xdg-pictures", "Pictures"), ("xdg-music", "Music"), ("xdg-videos", "Videos"),
    ("xdg-desktop", "Desktop"), ("xdg-config", "Config"), ("xdg-cache", "Cache"), ("xdg-data", "Data"),
];

/// In-memory state for the open permission editor: the selected app, its default
/// permissions, a working copy (defaults + edits) used to render, and staged
/// flags awaiting Apply (system scope only).
#[derive(Default)]
struct PermCtx {
    id: String,
    scope_system: bool,
    def: fperm::KeyFile,
    working: fperm::KeyFile,
    pending: Vec<String>,
}

/// Read an app's default (metadata) and effective (--show-permissions) keyfiles.
fn read_app_keyfiles(id: &str) -> (fperm::KeyFile, fperm::KeyFile) {
    let run = |args: &[&str]| {
        std::process::Command::new("flatpak")
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default()
    };
    let meta = run(&["info", "-m", id]);
    let eff = run(&["info", "--show-permissions", id]);
    (fperm::parse_keyfile(&meta), fperm::parse_keyfile(&eff))
}

fn perm_toggles(list: &[(&str, &str)], ctx_key: &str, def: &fperm::KeyFile, eff: &fperm::KeyFile) -> Vec<PermToggle> {
    list.iter()
        .map(|(k, l)| {
            let value = fperm::has_token(eff, ctx_key, k);
            let overridden = value != fperm::has_token(def, ctx_key, k);
            PermToggle { key: (*k).into(), label: (*l).into(), value, overridden }
        })
        .collect()
}

fn perm_fs_custom(def: &fperm::KeyFile, eff: &fperm::KeyFile) -> Vec<PermEntry> {
    let fixed: std::collections::HashSet<&str> = PERM_FS_FIXED.iter().map(|(k, _)| *k).collect();
    let def_toks = fperm::list_tokens(def, "filesystems");
    fperm::list_tokens(eff, "filesystems")
        .into_iter()
        .filter_map(|tok| {
            let (path, mode) = match tok.rsplit_once(':') {
                Some((p, m)) if ["ro", "rw", "create"].contains(&m) => (p.to_string(), m.to_string()),
                _ => (tok.clone(), String::new()),
            };
            if fixed.contains(path.as_str()) {
                return None;
            }
            let overridden = !def_toks.contains(&tok);
            Some(PermEntry { value: path.into(), mode: mode.into(), overridden })
        })
        .collect()
}

fn perm_bus(section: &str, def: &fperm::KeyFile, eff: &fperm::KeyFile) -> Vec<PermEntry> {
    let d = fperm::bus_entries(def, section);
    fperm::bus_entries(eff, section)
        .into_iter()
        .map(|(name, policy)| {
            let overridden = d.get(&name) != Some(&policy);
            PermEntry { value: name.into(), mode: policy.into(), overridden }
        })
        .collect()
}

fn perm_env(def: &fperm::KeyFile, eff: &fperm::KeyFile) -> Vec<PermEntry> {
    let d = fperm::env_entries(def);
    fperm::env_entries(eff)
        .into_iter()
        .map(|(k, v)| {
            let overridden = d.get(&k) != Some(&v);
            PermEntry { value: k.into(), mode: v.into(), overridden }
        })
        .collect()
}

fn perm_persist(def: &fperm::KeyFile, eff: &fperm::KeyFile) -> Vec<PermEntry> {
    let d: std::collections::HashSet<String> = fperm::list_tokens(def, "persistent").into_iter().collect();
    fperm::list_tokens(eff, "persistent")
        .into_iter()
        .map(|p| {
            let overridden = !d.contains(&p);
            PermEntry { value: p.into(), mode: SharedString::new(), overridden }
        })
        .collect()
}

/// Push all permission models for (def, working) into the window.
fn set_perm_models(w: &MainWindow, def: &fperm::KeyFile, working: &fperm::KeyFile) {
    w.set_perm_shares(ModelRc::new(VecModel::from(perm_toggles(PERM_SHARES, "shared", def, working))));
    w.set_perm_sockets(ModelRc::new(VecModel::from(perm_toggles(PERM_SOCKETS, "sockets", def, working))));
    w.set_perm_devices(ModelRc::new(VecModel::from(perm_toggles(PERM_DEVICES, "devices", def, working))));
    w.set_perm_features(ModelRc::new(VecModel::from(perm_toggles(PERM_FEATURES, "features", def, working))));
    w.set_perm_filesystems(ModelRc::new(VecModel::from(perm_toggles(PERM_FS_FIXED, "filesystems", def, working))));
    w.set_perm_filesystems_custom(ModelRc::new(VecModel::from(perm_fs_custom(def, working))));
    w.set_perm_session_bus(ModelRc::new(VecModel::from(perm_bus(fperm::SESSION_BUS, def, working))));
    w.set_perm_system_bus(ModelRc::new(VecModel::from(perm_bus(fperm::SYSTEM_BUS, def, working))));
    w.set_perm_env(ModelRc::new(VecModel::from(perm_env(def, working))));
    w.set_perm_persist(ModelRc::new(VecModel::from(perm_persist(def, working))));
}

// Working-keyfile mutations (mirror what `flatpak override` does, for instant UI).
fn kf_ctx_set(kf: &mut fperm::KeyFile, key: &str, token: &str, on: bool) {
    let mut toks = fperm::list_tokens(kf, key);
    toks.retain(|t| t != token);
    if on {
        toks.push(token.to_string());
    }
    kf.entry(fperm::CTX.to_string()).or_default().insert(key.to_string(), toks.join(";"));
}
fn kf_fs_add(kf: &mut fperm::KeyFile, path: &str, mode: &str) {
    let mut toks = fperm::list_tokens(kf, "filesystems");
    toks.retain(|t| t.rsplit_once(':').map(|(p, _)| p).unwrap_or(t.as_str()) != path);
    let entry = if mode.is_empty() || mode == "rw" { path.to_string() } else { format!("{path}:{mode}") };
    toks.push(entry);
    kf.entry(fperm::CTX.to_string()).or_default().insert("filesystems".into(), toks.join(";"));
}
fn kf_fs_remove(kf: &mut fperm::KeyFile, path: &str) {
    let mut toks = fperm::list_tokens(kf, "filesystems");
    toks.retain(|t| t.rsplit_once(':').map(|(p, _)| p).unwrap_or(t.as_str()) != path);
    kf.entry(fperm::CTX.to_string()).or_default().insert("filesystems".into(), toks.join(";"));
}
fn kf_bus_set(kf: &mut fperm::KeyFile, section: &str, name: &str, present: bool) {
    let s = kf.entry(section.to_string()).or_default();
    if present {
        s.insert(name.to_string(), "talk".to_string());
    } else {
        s.remove(name);
    }
}
fn kf_env_set(kf: &mut fperm::KeyFile, key: &str, val: Option<&str>) {
    let s = kf.entry(fperm::ENVIRONMENT.to_string()).or_default();
    match val {
        Some(v) => { s.insert(key.to_string(), v.to_string()); }
        None => { s.remove(key); }
    }
}
fn kf_persist_add(kf: &mut fperm::KeyFile, path: &str) {
    let mut toks = fperm::list_tokens(kf, "persistent");
    if !toks.iter().any(|t| t == path) {
        toks.push(path.to_string());
    }
    kf.entry(fperm::CTX.to_string()).or_default().insert("persistent".into(), toks.join(";"));
}

/// Run a user-scope `flatpak override` with one flag (fire-and-forget).
fn apply_user_override(app_id: &str, flag: &str) {
    let argv = fperm::override_argv(false, app_id, &[flag.to_string()]);
    let _ = std::process::Command::new("flatpak").args(&argv).status();
}

/// Installed flatpaks as left-list rows for the permission editor.
fn perm_app_list() -> Vec<PermApp> {
    load_installed_flatpaks()
        .into_iter()
        .map(|p| PermApp {
            id: p.name.clone(),
            name: p.display_name.clone(),
            initial: p.required_by.clone(),
            system: false,
        })
        .collect()
}

/// Read an app's permissions off disk and push them into the editor (background).
fn perm_load(weak: slint::Weak<MainWindow>, ctx: Arc<Mutex<PermCtx>>, id: String) {
    thread::spawn(move || {
        let (def, eff) = read_app_keyfiles(&id);
        {
            let mut c = ctx.lock().unwrap();
            c.id = id.clone();
            c.def = def.clone();
            c.working = eff.clone();
            c.pending.clear();
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                set_perm_models(&w, &def, &eff);
                w.set_perm_loading(false);
                w.set_perm_dirty(false);
            }
        });
    });
}

/// After an in-memory edit: rebuild models, then stage (system) or apply (user).
fn perm_after_edit(w: &MainWindow, ctx: &Arc<Mutex<PermCtx>>, flag: String) {
    let (def, working, system, id) = {
        let c = ctx.lock().unwrap();
        (c.def.clone(), c.working.clone(), c.scope_system, c.id.clone())
    };
    set_perm_models(w, &def, &working);
    if system {
        ctx.lock().unwrap().pending.push(flag);
        w.set_perm_dirty(true);
    } else {
        thread::spawn(move || apply_user_override(&id, &flag));
    }
}

// ---- Transaction history ----

/// Prettify a pacman.log timestamp `2026-06-25T10:00:00+0000` -> `2026-06-25 10:00:00`.
fn pretty_when(raw: &str) -> String {
    let no_tz = raw.split(['+']).next().unwrap_or(raw);
    no_tz.replace('T', " ")
}

fn txn_summary(t: &alpmhist::Transaction) -> String {
    use alpmhist::ActionKind::*;
    let mut parts = Vec::new();
    let u = t.count(Upgraded);
    let i = t.count(Installed);
    let d = t.count(Downgraded);
    let r = t.count(Removed);
    if u > 0 { parts.push(format!("{u} upgraded")); }
    if i > 0 { parts.push(format!("{i} installed")); }
    if d > 0 { parts.push(format!("{d} downgraded")); }
    if r > 0 { parts.push(format!("{r} removed")); }
    if parts.is_empty() { "no changes".to_string() } else { parts.join(", ") }
}

/// Locate a signed package file for an exact old version: local cache first, then
/// the Arch Linux Archive (downloaded with its .sig). Returns the file path.
fn resolve_old_pkg(name: &str, ver: &str) -> Option<String> {
    let cache = "/var/cache/pacman/pkg";
    for arch in ["x86_64", "any"] {
        let p = format!("{}/{}", cache, alpmhist::cache_filename(name, ver, arch));
        if Path::new(&p).exists() {
            return Some(p);
        }
    }
    // Any-arch glob in the cache.
    if let Ok(rd) = std::fs::read_dir(cache) {
        let prefix = format!("{name}-{ver}-");
        for e in rd.flatten() {
            let f = e.file_name().to_string_lossy().to_string();
            if f.starts_with(&prefix) && f.ends_with(".pkg.tar.zst") {
                return Some(e.path().to_string_lossy().to_string());
            }
        }
    }
    // Arch Linux Archive (download package + signature for pacman to verify).
    let tmp = std::env::temp_dir().join("xpm-rollback");
    let _ = std::fs::create_dir_all(&tmp);
    for arch in ["x86_64", "any"] {
        let url = alpmhist::ala_url(name, ver, arch);
        let file = tmp.join(alpmhist::cache_filename(name, ver, arch));
        let file_s = file.to_string_lossy().to_string();
        let ok = std::process::Command::new("curl")
            .args(["-fsSL", "-o", &file_s, &url])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            let _ = std::process::Command::new("curl")
                .args(["-fsSL", "-o", &format!("{file_s}.sig"), &format!("{url}.sig")])
                .status();
            return Some(file_s);
        }
    }
    None
}

/// Note for the rollback warning dialog reflecting detected snapshot tools.
fn snapshot_note() -> String {
    let have = |c: &str| {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {c}"))
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    let mut found = Vec::new();
    if have("snapper") { found.push("Snapper"); }
    if have("timeshift") { found.push("Timeshift"); }
    if found.is_empty() {
        "No snapshot tool (Snapper/Timeshift) detected - consider taking a manual backup first.".to_string()
    } else {
        format!("Detected: {} - take a snapshot before proceeding.", found.join(" + "))
    }
}

/// Load pacman.log into the history modal (and show it).
fn load_history(weak: slint::Weak<MainWindow>, store: Arc<Mutex<Vec<alpmhist::Transaction>>>) {
    if let Some(w) = weak.upgrade() {
        w.set_history_loading(true);
        w.set_history_selected(-1);
        w.set_history_actions(ModelRc::new(VecModel::from(Vec::<HistAction>::new())));
        w.set_show_history_modal(true);
    }
    thread::spawn(move || {
        let text = std::fs::read_to_string("/var/log/pacman.log").unwrap_or_default();
        let mut txns = alpmhist::parse_log(&text);
        txns.truncate(150);
        let rows: Vec<HistTxn> = txns
            .iter()
            .map(|t| HistTxn {
                when: pretty_when(&t.when).into(),
                command: t.command.as_str().into(),
                summary: txn_summary(t).into(),
                rollbackable: !t.upgraded_targets().is_empty(),
            })
            .collect();
        *store.lock().unwrap() = txns;
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(w) = weak.upgrade() {
                w.set_history_txns(ModelRc::new(VecModel::from(rows)));
                w.set_history_loading(false);
            }
        });
    });
}

fn main() {
    // SAFETY: runs first in main, before any thread is spawned.
    unsafe {
        if std::env::var_os("SLINT_BACKEND").is_none() {
            std::env::set_var("SLINT_BACKEND", "qt");
        }

        if std::env::var("QT_PLUGIN_PATH").map(|p| p.is_empty()).unwrap_or(true) {
            std::env::set_var("QT_PLUGIN_PATH", "/usr/lib/qt6/plugins:/usr/lib/x86_64-linux-gnu/qt6/plugins");
        }

        let current_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        if !current_path.contains("/usr/lib/qt6") {
            let new_path = if current_path.is_empty() {
                "/usr/lib/qt6:/usr/lib/x86_64-linux-gnu/qt6".to_string()
            } else {
                format!("{}:/usr/lib/qt6:/usr/lib/x86_64-linux-gnu/qt6", current_path)
            };
            std::env::set_var("LD_LIBRARY_PATH", new_path);
        }
    }

    let subscriber = FmtSubscriber::builder()
    .with_max_level(Level::INFO)
    .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set subscriber");

    info!("Starting xPackageManager");

    let args: Vec<String> = std::env::args().collect();
    let local_package_path = args.iter().skip(1)
        .find(|arg| is_arch_package(arg.as_str()))
        .cloned();
    let flatpak_file_arg = args.iter().skip(1)
        .find(|arg| flatpak_file_kind(arg.as_str()).is_some())
        .and_then(|arg| std::fs::canonicalize(arg).ok())
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned());

    if let Some(ref path) = local_package_path {
        info!("Opening local package: {}", path);
    }

    let _instance_lock = match acquire_instance_lock() {
        Some(f) => f,
        None => {
            info!("Another instance is already running - bringing it to foreground");
            signal_existing_instance(flatpak_file_arg.as_deref());
            return;
        }
    };

    let window = MainWindow::new().expect("Failed to create window");
    window.set_app_version(SharedString::from(env!("CARGO_PKG_VERSION")));

    if let Some(locale) = sys_locale::get_locale() {
        let lang = locale.split(['_', '-', '.']).next().unwrap_or("en").to_ascii_lowercase();
        let full = locale.replace('-', "_").to_ascii_lowercase();
        let result = slint::select_bundled_translation(&full)
            .or_else(|_| slint::select_bundled_translation(&lang));
        match result {
            Ok(()) => info!("Translation loaded for locale {} ({})", locale, lang),
            Err(e) => info!("No translation for locale {}: {:?}", locale, e),
        }
    }

    let (tx, rx) = mpsc::channel::<UiMessage>();
    let rx = Rc::new(RefCell::new(rx));

    let appimage_entries: Rc<RefCell<Vec<AppImageEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let flatpak_app_store: Arc<Mutex<Vec<CachedRemoteApp>>> = Arc::new(Mutex::new(Vec::new()));
    let flatpak_loaded_remote: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

    let icon_index: Rc<RefCell<HashMap<String, std::path::PathBuf>>> = Rc::new(RefCell::new(HashMap::new()));
    let icon_cache: Rc<RefCell<HashMap<String, slint::Image>>> = Rc::new(RefCell::new(HashMap::new()));
    let icon_pending: Rc<RefCell<std::collections::HashSet<String>>> = Rc::new(RefCell::new(std::collections::HashSet::new()));
    {
        let (jobs, job_rx) = mpsc::channel::<(String, std::path::PathBuf)>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..3 {
            let (rx, tx_done) = (job_rx.clone(), tx.clone());
            thread::spawn(move || {
                while let Ok((name, path)) = { rx.lock().unwrap().recv() } {
                    let _ = tx_done.send(UiMessage::IconDecoded { name, pixels: icons::decode_file(&path) });
                }
            });
        }

        let (index, cache, pending) = (icon_index.clone(), icon_cache.clone(), icon_pending.clone());
        let (store, remote) = (flatpak_app_store.clone(), flatpak_loaded_remote.clone());
        let appimages = appimage_entries.clone();
        window.global::<IconStore>().on_lookup(move |name, _epoch| {
            if name.is_empty() {
                return slint::Image::default();
            }
            if let Some(img) = cache.borrow().get(name.as_str()) {
                return img.clone();
            }
            let path = if let Some(id) = name.strip_prefix("flatpak:") {
                icons::flatpak_icon(&store.lock().unwrap(), &remote.lock().unwrap(), id)
            } else if let Some(id) = name.strip_prefix("appimage:") {
                appimages
                    .borrow()
                    .iter()
                    .find(|e| e.name == id)
                    .and_then(|e| e.icon_path.as_ref())
                    .map(std::path::PathBuf::from)
                    .filter(|p| p.is_file())
            } else {
                index.borrow().get(name.as_str()).cloned()
            };
            // Flatpak stores and AppImage entries load late; only native misses are final.
            let final_miss = !name.contains(':');
            let Some(path) = path else {
                if final_miss {
                    cache.borrow_mut().insert(name.to_string(), slint::Image::default());
                }
                return slint::Image::default();
            };
            if icons::is_svg(&path) {
                let img = slint::Image::load_from_path(&path).unwrap_or_default();
                cache.borrow_mut().insert(name.to_string(), img.clone());
                return img;
            }
            if pending.borrow_mut().insert(name.to_string()) {
                let _ = jobs.send((name.to_string(), path));
            }
            slint::Image::default()
        });
        let tx_icons = tx.clone();
        thread::spawn(move || {
            let root = Path::new("/");
            let cache = icons::catalog_cache_dir();
            let extra = icons::cached_catalogs(&cache);
            let _ = tx_icons.send(UiMessage::IconIndexReady(icons::build_index(root, &extra)));
            if icons::ensure_catalog_cache(root, &cache) {
                let _ = tx_icons.send(UiMessage::IconIndexReady(icons::build_index(root, &extra)));
            }
        });
    }

    let appimage_catalog: Arc<Mutex<Vec<CatalogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let appimage_dir_state: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let appimage_sources_state: Arc<Mutex<Vec<AppImageFeed>>> = Arc::new(Mutex::new(Vec::new()));
    let appimage_updates: Rc<RefCell<std::collections::HashSet<String>>> =
        Rc::new(RefCell::new(std::collections::HashSet::new()));
    let appimage_enabled_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));

    listen_for_instance_signals(window.as_weak());

    let terminal_input_sender: Arc<Mutex<Option<mpsc::Sender<String>>>> = Arc::new(Mutex::new(None));
    let terminal_child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let conflict_context: ConflictCtx = Arc::new(Mutex::new(None));
    // Which remote the in-memory store currently holds, so switching remotes in
    // the browse dropdown reloads instead of reusing the wrong remote's apps.
    let flatpak_installed_ids: Arc<Mutex<std::collections::HashSet<String>>> = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let flatpak_filter_serial: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(0));

    const FLATPAK_PAGE_SIZE: usize = BROWSE_PAGE_SIZE;

    if let Some(cache) = load_package_cache() {
        let installed: Vec<PackageData> = cache.installed.iter().map(cached_to_pkg).collect();
        let (flatpak_updates, updates): (Vec<PackageData>, Vec<PackageData>) =
            cache.updates.iter().map(cached_to_pkg).partition(|p| p.backend == 1);
        let flatpak: Vec<PackageData> = cache.flatpak.iter().map(cached_to_pkg).collect();
        let stats = StatsData {
            pacman_count: cache.pacman_count,
            flatpak_count: cache.flatpak_count,
            orphan_count: cache.orphan_count,
            cache_size: SharedString::from(cache.cache_size.as_str()),
        };
        let page_size_early = 50usize;
        let page: Vec<PackageData> = installed.iter().take(page_size_early).cloned().collect();
        let total = installed.len().div_ceil(page_size_early).max(1) as i32;
        window.set_installed_packages(ModelRc::new(VecModel::from(page)));
        window.set_total_pages(total);
        window.set_update_packages(ModelRc::new(VecModel::from(updates)));
        window.set_flatpak_update_packages(ModelRc::new(VecModel::from(flatpak_updates)));
        window.set_flatpak_packages(ModelRc::new(VecModel::from(flatpak)));
        window.set_stats(stats);
    }
    window.set_loading(false);

    let selected_packages: Rc<RefCell<Vec<(String, i32, bool)>>> = Rc::new(RefCell::new(Vec::new()));

    let page_size: i32 = 50;
    let full_installed: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));
    let full_installed_flatpaks: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));
    let full_installed_grouped: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));
    let repo_packages_full: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));

    let tx_load = tx.clone();
    let tx_search = tx.clone();


    let timer = Timer::default();
    let window_weak = window.as_weak();
    let rx_clone = rx.clone();
    let tx_timer = tx.clone();
    let full_installed_timer = full_installed.clone();
    let full_installed_flatpaks_timer = full_installed_flatpaks.clone();
    let full_installed_grouped_timer = full_installed_grouped.clone();
    let repo_full_timer = repo_packages_full.clone();
    let filter_serial_timer = flatpak_filter_serial.clone();
    let conflict_ctx_timer = conflict_context.clone();
    let flatpak_ids_timer = flatpak_installed_ids.clone();
    let flatpak_store_timer = flatpak_app_store.clone();
    let notified_updates = Rc::new(std::cell::Cell::new(false));
    let awaiting_appimage_check = Rc::new(std::cell::Cell::new(false));
    let awaiting_appimage_check_dispatch = awaiting_appimage_check.clone();
    let awaiting_appimage_check_init = awaiting_appimage_check.clone();
    let notified_updates_dispatch = notified_updates.clone();
    let cat_dispatch = appimage_catalog.clone();
    let updates_dispatch = appimage_updates.clone();
    let icon_index_dispatch = icon_index.clone();
    let icon_cache_dispatch = icon_cache.clone();
    let icon_pending_dispatch = icon_pending.clone();
    let ai_entries_dispatch = appimage_entries.clone();

    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(50), move || {
        if let Some(window) = window_weak.upgrade() {
            let _flush_now = false;

            while let Ok(msg) = rx_clone.borrow_mut().try_recv() {
                match msg {
                    UiMessage::PackagesLoaded { installed, updates, flatpak_updates, flatpak, stats } => {
                        *full_installed_timer.borrow_mut() = installed;
                        let ps = page_size as usize;
                        let inst = full_installed_timer.borrow();
                        let total = inst.len().div_ceil(ps).max(1) as i32;
                        let page: Vec<PackageData> = inst.iter().take(ps).cloned().collect();
                        window.set_installed_packages(ModelRc::new(VecModel::from(page)));
                        window.set_current_page(0);
                        window.set_total_pages(total);
                        drop(inst);
                        window.set_update_packages(ModelRc::new(VecModel::from(updates)));
                        window.set_flatpak_update_packages(ModelRc::new(VecModel::from(flatpak_updates)));
                        window.set_flatpak_packages(ModelRc::new(VecModel::from(flatpak)));
                        window.set_stats(stats);
                        let full_for_grp: Vec<PackageData> = full_installed_timer.borrow().clone();
                        let grouped = group_installed_by_repo(full_for_grp);
                        *full_installed_grouped_timer.borrow_mut() = grouped.clone();
                        window.set_installed_grouped(ModelRc::new(VecModel::from(grouped)));
                        window.set_loading(false);
                        if !awaiting_appimage_check.get() {
                            notify_updates_once(&window, &notified_updates);
                        }
                    }
                    UiMessage::SearchResults(results) => {
                        let installed: Vec<PackageData> = results.iter().filter(|p| p.installed).cloned().collect();
                        let available: Vec<PackageData> = results.iter().filter(|p| !p.installed).cloned().collect();
                        window.set_search_installed(ModelRc::new(VecModel::from(installed)));
                        window.set_search_available(ModelRc::new(VecModel::from(available)));
                        window.set_loading(false);
                    }
                    UiMessage::SetLoading(loading) => {
                        window.set_loading(loading);
                    }
                    UiMessage::SetBusy(busy) => {
                        window.set_busy(busy);
                    }
                    UiMessage::SetStatus(status) => {
                        window.set_status_message(SharedString::from(&status));
                    }
                    UiMessage::SetProgress(value) => {
                        window.set_progress(value);
                    }
                    UiMessage::SetProgressText(text) => {
                        window.set_progress_text(SharedString::from(&text));
                    }
                    UiMessage::SetTerminalIsUpgrade(val) => {
                        window.set_terminal_is_upgrade(val);
                    }
                    UiMessage::ShowProgressPopup(title) => {
                        window.set_progress_popup_title(SharedString::from(&title));
                        window.set_progress_popup_percent(0);
                        window.set_progress_popup_stage(SharedString::from("Starting..."));
                        window.set_progress_popup_output(SharedString::from(""));
                        window.set_progress_popup_show_input(false);
                        window.set_progress_popup_show_buttons(false);
                        window.set_progress_popup_prompt(SharedString::from(""));
                        window.set_progress_popup_done(false);
                        window.set_progress_popup_success(false);
                        window.set_progress_show_details(false);
                        window.set_progress_error_summary(SharedString::from(""));
                        window.set_progress_input_focus_pending(false);
                        window.set_progress_popup_cancellable(false);
                        window.set_show_progress_popup(true);
                    }
                    UiMessage::ProgressMode { cancellable } => {
                        window.set_progress_popup_cancellable(cancellable);
                    }
                    UiMessage::IconDecoded { name, pixels } => {
                        icon_pending_dispatch.borrow_mut().remove(&name);
                        let img = pixels.map(icons::Pixels::into_image).unwrap_or_default();
                        icon_cache_dispatch.borrow_mut().insert(name, img);
                        let store = window.global::<IconStore>();
                        store.set_epoch(store.get_epoch() + 1);
                    }
                    UiMessage::IconIndexReady(index) => {
                        *icon_index_dispatch.borrow_mut() = index;
                        icon_cache_dispatch.borrow_mut().clear();
                        icon_pending_dispatch.borrow_mut().clear();
                        let store = window.global::<IconStore>();
                        store.set_epoch(store.get_epoch() + 1);
                    }
                    UiMessage::ProgressStage(stage) => {
                        window.set_progress_popup_stage(SharedString::from(&stage));
                    }
                    UiMessage::ProgressOutput(text) => {
                        window.set_progress_popup_output(SharedString::from(&text));
                    }
                    UiMessage::ProgressPrompt(prompt) => {
                        window.set_progress_popup_prompt(SharedString::from(&prompt));
                        window.set_progress_popup_show_input(true);
                        window.set_progress_input_focus_pending(true);
                    }
                    UiMessage::ProgressHidePrompt => {
                        window.set_progress_popup_show_input(false);
                        window.set_progress_popup_show_buttons(false);
                        window.set_progress_popup_prompt(SharedString::from(""));
                    }
                    UiMessage::ProgressPromptButtons => {
                        window.set_progress_popup_show_buttons(true);
                        window.set_progress_popup_show_input(true);
                    }
                    UiMessage::ProgressErrorSummary(s) => {
                        window.set_progress_error_summary(SharedString::from(&s));
                    }
                    UiMessage::ProgressAutoExpand => {
                        window.set_progress_show_details(true);
                    }
                    UiMessage::OperationProgress(percent, stage) => {
                        window.set_progress_popup_percent(percent);
                        if !stage.is_empty() {
                            window.set_progress_popup_stage(SharedString::from(&stage));
                        }
                    }
                    UiMessage::OperationDone(success) => {
                        window.set_progress_popup_percent(100);
                        window.set_progress_popup_done(true);
                        window.set_progress_popup_success(success);
                        window.set_progress_popup_show_input(false);
                        window.set_progress_popup_prompt(SharedString::from(""));
                        if success {
                            if let Some((action, names, backend)) = conflict_ctx_timer.lock().unwrap().clone() {
                                let is_remove = action == "remove" || action == "bulk-remove";
                                let is_install = action == "install" || action == "bulk-install";
                                let new_installed = is_install;

                                if !names.is_empty() {
                                    let name_set: std::collections::HashSet<String> =
                                        names.iter().cloned().collect();
                                    let name_set_ref: std::collections::HashSet<&str> =
                                        name_set.iter().map(|s| s.as_str()).collect();

                                    let updated = flip_installed_in_model(
                                        window.get_search_available(), &name_set_ref, new_installed);
                                    window.set_search_available(updated);
                                    let updated = flip_installed_in_model(
                                        window.get_search_installed(), &name_set_ref, new_installed);
                                    window.set_search_installed(updated);
                                    let updated = flip_installed_in_model(
                                        window.get_repo_packages(), &name_set_ref, new_installed);
                                    window.set_repo_packages(updated);
                                    for p in repo_full_timer.borrow_mut().iter_mut() {
                                        if name_set_ref.contains(p.name.as_str()) {
                                            p.installed = new_installed;
                                        }
                                    }

                                    let updated = flip_installed_in_model(
                                        window.get_remote_apps(), &name_set_ref, new_installed);
                                    window.set_remote_apps(updated);

                                    if backend == 1 {
                                        if is_remove {
                                            let current: Vec<PackageData> = window.get_installed_flatpaks()
                                                .iter()
                                                .filter(|p| !name_set_ref.contains(p.name.as_str()))
                                                .collect();
                                            window.set_installed_flatpaks(ModelRc::new(VecModel::from(current)));
                                            let current: Vec<PackageData> = window.get_flatpak_packages()
                                                .iter()
                                                .filter(|p| !name_set_ref.contains(p.name.as_str()))
                                                .collect();
                                            window.set_flatpak_packages(ModelRc::new(VecModel::from(current)));
                                        }
                                        let cur_id = window.get_current_flatpak_id();
                                        if name_set_ref.contains(cur_id.as_str()) {
                                            window.set_flatpak_detail_installed(new_installed);
                                        }

                                        let mut all: Vec<PackageData> =
                                            window.get_flatpak_addons().iter().collect();
                                        all.extend(window.get_flatpak_addons_installed().iter());
                                        let mut changed = false;
                                        for a in all.iter_mut() {
                                            if name_set_ref.contains(a.name.as_str()) {
                                                a.installed = new_installed;
                                                changed = true;
                                            }
                                        }
                                        if changed {
                                            let (inst, uninst): (Vec<PackageData>, Vec<PackageData>) =
                                                all.into_iter().partition(|a| a.installed);
                                            let uninst_len = uninst.len();
                                            window.set_flatpak_addons_installed_count(inst.len() as i32);
                                            window.set_flatpak_addons_installed(ModelRc::new(VecModel::from(inst)));
                                            window.set_flatpak_addons(ModelRc::new(VecModel::from(uninst)));
                                            window.set_addon_selected(ModelRc::new(VecModel::from(vec![false; uninst_len])));
                                            window.set_addon_selected_count(0);
                                        }
                                    }

                                    if backend == 0 {
                                        if is_remove {
                                            {
                                                let mut inst = full_installed_timer.borrow_mut();
                                                inst.retain(|p| !name_set_ref.contains(p.name.as_str()));
                                            }
                                            {
                                                let mut grp = full_installed_grouped_timer.borrow_mut();
                                                grp.retain(|p| !name_set_ref.contains(p.name.as_str()));
                                            }
                                            let ps = page_size as usize;
                                            let inst = full_installed_timer.borrow();
                                            let total = inst.len().div_ceil(ps).max(1) as i32;
                                            let page: Vec<PackageData> = inst.iter().take(ps).cloned().collect();
                                            drop(inst);
                                            window.set_installed_packages(ModelRc::new(VecModel::from(page)));
                                            window.set_current_page(0);
                                            window.set_total_pages(total);
                                            let grp_snap: Vec<PackageData> = full_installed_grouped_timer.borrow().clone();
                                            window.set_installed_grouped(ModelRc::new(VecModel::from(grp_snap)));
                                        }
                                        let mut dpkg = window.get_repo_detail_pkg();
                                        if name_set_ref.contains(dpkg.name.as_str()) {
                                            dpkg.installed = new_installed;
                                            window.set_repo_detail_pkg(dpkg);
                                        }
                                    }
                                }
                            }
                            window.set_selected_count(0);
                        }
                        if success {
                            if window.get_setting_auto_clean_cache()
                                && let Some((ref action, _, _)) = *conflict_ctx_timer.lock().unwrap()
                                    && (action == "update-all" || action == "force-update-all") {
                                        let keep = window.get_setting_clean_keep_versions();
                                        thread::spawn(move || {
                                            let _ = std::process::Command::new("pkexec")
                                                .args(["paccache", "-rk", &keep.to_string()])
                                                .status();
                                        });
                                    }
                        let tx = tx_timer.clone();
                            let search_query = window.get_search_text().to_string();
                            let ids_ref = flatpak_ids_timer.clone();
                            let store_ref = flatpak_store_timer.clone();

                            thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().expect("Runtime");
                                rt.block_on(async {
                                    let new_ids = tokio::task::spawn_blocking(get_flatpak_installed_ids).await.unwrap_or_default();
                                    *ids_ref.lock().unwrap() = new_ids;
                                    let store_join = store_ref.clone();
                                    let ids_join = ids_ref.clone();
                                    tokio::join!(
                                        load_packages_async(&tx, false),
                                        async {
                                            if !search_query.is_empty() {
                                                search_packages_async(&tx, &search_query, store_join, ids_join).await;
                                            }
                                        }
                                    );
                                    let pkgs = tokio::task::spawn_blocking(load_installed_flatpaks).await.unwrap_or_default();
                                    let _ = tx.send(UiMessage::InstalledFlatpaksLoaded(pkgs));
                                    let _ = tx.send(UiMessage::ActivityLoaded(load_recent_activity()));
                                    let _ = tx.send(UiMessage::SysInfoLoaded(load_sys_info()));
                                });
                            });
                        }
                    }
                    UiMessage::ShowConflict { summary, can_force } => {
                        window.set_show_progress_popup(false);
                        window.set_conflict_summary(SharedString::from(&summary));
                        window.set_conflict_can_force(can_force);
                        window.set_show_conflict_dialog(true);
                    }
                    UiMessage::FlatpakDetailReady(detail) => {
                        let FlatpakDetail { app_id, name, summary, description, developer, version, version_date, license, url_homepage, url_bugtracker, url_translate, url_vcs, url_donation, url_help, categories, releases, captions } = *detail;
                        if window.get_current_flatpak_id() != app_id.as_str() {
                            continue;
                        }
                        window.set_flatpak_detail_name(SharedString::from(&name));
                        window.set_flatpak_detail_summary(SharedString::from(&summary));
                        window.set_flatpak_detail_description(SharedString::from(&description));
                        window.set_flatpak_detail_developer(SharedString::from(&developer));
                        window.set_flatpak_detail_version(SharedString::from(&version));
                        window.set_flatpak_detail_version_date(SharedString::from(&version_date));
                        window.set_flatpak_detail_license(SharedString::from(&license));
                        window.set_flatpak_detail_url_homepage(SharedString::from(&url_homepage));
                        window.set_flatpak_detail_url_bug(SharedString::from(&url_bugtracker));
                        window.set_flatpak_detail_url_translate(SharedString::from(&url_translate));
                        window.set_flatpak_detail_url_vcs(SharedString::from(&url_vcs));
                        window.set_flatpak_detail_url_donate(SharedString::from(&url_donation));
                        window.set_flatpak_detail_url_help(SharedString::from(&url_help));
                        window.set_flatpak_detail_tags(ModelRc::new(VecModel::from(
                            categories.iter().map(|c| SharedString::from(c.as_str())).collect::<Vec<_>>()
                        )));
                        window.set_flatpak_releases(ModelRc::new(VecModel::from(
                            releases.into_iter().map(|(version, date, notes)| ReleaseItem {
                                version: version.into(),
                                date: date.into(),
                                notes: notes.into(),
                            }).collect::<Vec<_>>()
                        )));
                        window.set_flatpak_shots(ModelRc::new(VecModel::from(
                            captions.into_iter().map(|caption| ShotItem {
                                image: slint::Image::default(),
                                caption: caption.into(),
                            }).collect::<Vec<_>>()
                        )));
                        window.set_flatpak_shot_index(0);
                        window.set_show_flatpak_detail(true);
                    }
                    UiMessage::FlatpakExtrasReady { app_id, extras } => {
                        if window.get_current_flatpak_id() == app_id.as_str() {
                            let size = |bytes: u64| if bytes > 0 { format_size(bytes) } else { String::new() };
                            window.set_flatpak_detail_download_size(SharedString::from(size(extras.download_size)));
                            window.set_flatpak_detail_installed_size(SharedString::from(size(extras.installed_size)));
                            window.set_flatpak_detail_installs(SharedString::from(
                                if extras.installs > 0 { flathub::format_count(extras.installs) } else { String::new() },
                            ));
                            window.set_flatpak_detail_verified(extras.verified);
                        }
                    }
                    UiMessage::ActivityLoaded(items) => {
                        window.set_activity_items(ModelRc::new(VecModel::from(items)));
                    }
                    UiMessage::SysInfoLoaded(info) => {
                        window.set_sys_info(info);
                    }
                    UiMessage::FlatpakRemotesLoaded(remotes) => {
                        window.set_flatpak_remotes(ModelRc::new(VecModel::from(
                            remotes.iter().map(|r| SharedString::from(r.as_str())).collect::<Vec<_>>()
                        )));
                        window.set_flatpak_remote_count(remotes.len() as i32);
                        // Keep the dropdown selection in step with the preload default
                        // (flathub when present, else the first remote).
                        if window.get_selected_remote().is_empty() {
                            let default = remotes.iter().find(|r| r.as_str() == "flathub")
                                .or_else(|| remotes.first());
                            if let Some(d) = default {
                                window.set_selected_remote(SharedString::from(d.as_str()));
                            }
                        }
                    }
                    UiMessage::RemoteAppsFiltered { serial, apps, total_matches } => {
                        let current = filter_serial_timer.load(std::sync::atomic::Ordering::Relaxed);
                        if serial == u64::MAX || serial == current {
                            if serial == u64::MAX {
                                window.set_flatpak_page(0);
                            }
                            window.set_flatpak_total_matches(total_matches as i32);
                            window.set_remote_apps(ModelRc::new(VecModel::from(apps)));
                            window.set_remote_apps_loading(false);
                            window.set_flatpak_store_ready(true);
                        }
                    }
                    UiMessage::FlatpakScreenshotReady { app_id, index, path } => {
                        if window.get_current_flatpak_id() == app_id.as_str()
                            && let Ok(img) = slint::Image::load_from_path(std::path::Path::new(&path))
                        {
                            let shots = window.get_flatpak_shots();
                            if let Some(mut shot) = shots.row_data(index) {
                                shot.image = img;
                                shots.set_row_data(index, shot);
                            }
                        }
                    }
                    UiMessage::FlatpakIconReady(path) => {
                        if let Ok(img) = slint::Image::load_from_path(std::path::Path::new(&path)) {
                            window.set_flatpak_detail_icon(img);
                        }
                    }
                    UiMessage::FlatpakAddonsReady(addons) => {
                        let (installed_list, uninstalled_list): (Vec<PackageData>, Vec<PackageData>) =
                            addons.into_iter().partition(|a| a.installed);
                        let installed_count = installed_list.len() as i32;
                        let uninstalled_len = uninstalled_list.len();
                        window.set_flatpak_addons_installed_count(installed_count);
                        window.set_flatpak_addons_installed(ModelRc::new(VecModel::from(installed_list)));
                        window.set_flatpak_addons(ModelRc::new(VecModel::from(uninstalled_list)));
                        window.set_addon_selected(ModelRc::new(VecModel::from(vec![false; uninstalled_len])));
                        window.set_addon_selected_count(0);
                    }
                    UiMessage::PacmanReposLoaded(repos) => {
                        window.set_pacman_repos(ModelRc::new(VecModel::from(
                            repos.iter().map(|r| SharedString::from(r.as_str())).collect::<Vec<_>>()
                        )));
                    }
                    UiMessage::RepoPackagesLoaded(pkgs) => {
                        window.set_repo_search(SharedString::from(""));
                        render_repo_page(&window, &pkgs, 0);
                        *repo_full_timer.borrow_mut() = pkgs;
                        window.set_repo_loading(false);
                    }
                    UiMessage::RepoPkgDetail(desc) => {
                        window.set_repo_detail_description(SharedString::from(&desc));
                        window.set_repo_detail_loading(false);
                    }
                    UiMessage::PkgInfoLoaded(files) => {
                        window.set_pkg_info_files(SharedString::from(&files));
                        window.set_pkg_info_loading(false);
                    }
                    UiMessage::InstalledFlatpaksLoaded(pkgs) => {
                        *full_installed_flatpaks_timer.borrow_mut() = pkgs.clone();
                        window.set_installed_flatpaks(ModelRc::new(VecModel::from(pkgs)));
                    }
                    UiMessage::InstalledAppImagesLoaded(entries) => {
                        {
                            let live: std::collections::HashSet<&str> =
                                entries.iter().map(|e| e.name.as_str()).collect();
                            let mut up = updates_dispatch.borrow_mut();
                            up.retain(|n| live.contains(n.as_str()));
                        }
                        let updates = updates_dispatch.borrow();
                        let cards: Vec<AppImageInstalled> = entries
                            .iter()
                            .map(|e| entry_to_installed_card(e, &updates))
                            .collect();
                        window.set_installed_appimages(ModelRc::new(VecModel::from(cards)));
                        window.set_appimage_update_packages(ModelRc::new(VecModel::from(
                            build_appimage_update_rows(&entries, &updates),
                        )));
                        drop(updates);
                        *ai_entries_dispatch.borrow_mut() = entries;
                    }
                    UiMessage::AppImageUpdateCleared(id) => {
                        let mut up = updates_dispatch.borrow_mut();
                        up.remove(&id);
                        drop(up);
                        let updates = updates_dispatch.borrow();
                        window.set_appimage_update_packages(ModelRc::new(VecModel::from(
                            build_appimage_update_rows(&ai_entries_dispatch.borrow(), &updates),
                        )));
                    }
                    UiMessage::AppImageUpdatesChecked(names) => {
                        let first_check = awaiting_appimage_check_dispatch.replace(false);
                        *updates_dispatch.borrow_mut() = names.into_iter().collect();
                        let updates = updates_dispatch.borrow();
                        let model = window.get_installed_appimages();
                        let refreshed: Vec<AppImageInstalled> = (0..model.row_count())
                            .filter_map(|i| model.row_data(i))
                            .map(|mut c| {
                                c.update_available = updates.contains(c.id.as_str());
                                c
                            })
                            .collect();
                        window.set_installed_appimages(ModelRc::new(VecModel::from(refreshed)));
                        window.set_appimage_update_packages(ModelRc::new(VecModel::from(
                            build_appimage_update_rows(&ai_entries_dispatch.borrow(), &updates),
                        )));
                        if first_check && !window.get_loading() {
                            notify_updates_once(&window, &notified_updates_dispatch);
                        }
                    }
                    UiMessage::AppImageCatalogReady => {
                        let page = window.get_appimage_page().max(0) as usize;
                        let (cards, total) = filter_catalog(
                            &cat_dispatch.lock().unwrap(),
                            window.get_appimage_search().as_str(),
                            window.get_selected_appimage_source().as_str(),
                            &installed_github_map(&ai_entries_dispatch.borrow()),
                            page,
                        );
                        window.set_appimage_catalog_loading(false);
                        window.set_appimage_catalog_total(total as i32);
                        window.set_appimage_page(clamp_appimage_page(page, total) as i32);
                        window.set_catalog_appimages(ModelRc::new(VecModel::from(cards)));
                    }
                    UiMessage::AppImageCardsRefresh => {
                        let page = window.get_appimage_page().max(0) as usize;
                        let (cards, total) = filter_catalog(
                            &cat_dispatch.lock().unwrap(),
                            window.get_appimage_search().as_str(),
                            window.get_selected_appimage_source().as_str(),
                            &installed_github_map(&ai_entries_dispatch.borrow()),
                            page,
                        );
                        window.set_appimage_catalog_total(total as i32);
                        window.set_appimage_page(clamp_appimage_page(page, total) as i32);
                        window.set_catalog_appimages(ModelRc::new(VecModel::from(cards)));
                    }
                    UiMessage::AppImageCatalogLoading(v) => {
                        window.set_appimage_catalog_loading(v);
                    }
                    UiMessage::AppImageIconReady { github, path } => {
                        if let Ok(img) = slint::Image::load_from_path(std::path::Path::new(&path)) {
                            set_appimage_card_icon(&window, &github, img);
                        }
                    }
                    UiMessage::AppImageIconPixels { github, pixels } => {
                        set_appimage_card_icon(&window, &github, pixels.into_image());
                    }
                    UiMessage::DepTreeLoaded { deps, reqby, root_version } => {
                        window.set_dep_tree_loading(false);
                        window.set_dep_tree_root_version(SharedString::from(&root_version));
                        window.set_dep_tree_nodes(ModelRc::new(VecModel::from(deps)));
                        window.set_dep_reqby_nodes(ModelRc::new(VecModel::from(reqby)));
                    }
                    UiMessage::ArchNewsLoading => {
                        window.set_arch_news_loading(true);
                    }
                    UiMessage::ArchNewsLoaded(items) => {
                        window.set_arch_news_loading(false);
                        window.set_arch_news_items(ModelRc::new(VecModel::from(items)));
                    }
                    UiMessage::ShowWarning { message, chaotic_aur } => {
                        window.set_warning_popup_message(SharedString::from(&message));
                        window.set_warning_popup_chaotic_aur(chaotic_aur);
                        window.set_show_warning_popup(true);
                    }
                    UiMessage::RepoListLoaded(repos) => {
                        let entries: Vec<RepoEntry> = repos.iter().map(|(name, _, server)| RepoEntry {
                            name: SharedString::from(name.as_str()),
                            server: SharedString::from(server.as_str()),
                        }).collect();
                        let enabled: Vec<bool> = repos.iter().map(|(_, en, _)| *en).collect();
                        window.set_repo_mgr_list(ModelRc::new(VecModel::from(entries)));
                        window.set_repo_mgr_enabled(ModelRc::new(VecModel::from(enabled)));
                        window.set_repo_mgr_loading(false);
                    }
                    UiMessage::PacmanOptsLoaded(opts) => {
                        window.set_opt_color(opts.color);
                        window.set_opt_love_candy(opts.love_candy);
                        window.set_opt_verbose_pkg_lists(opts.verbose_pkg_lists);
                        window.set_opt_disable_dl_timeout(opts.disable_dl_timeout);
                        window.set_opt_check_space(opts.check_space);
                        window.set_opt_disable_sandbox(opts.disable_sandbox);
                        window.set_opt_no_progress_bar(opts.no_progress_bar);
                        window.set_opt_use_syslog(opts.use_syslog);
                        window.set_opt_clean_method(opts.clean_method);
                    }
                    UiMessage::UpdateCacheSize(size) => {
                        let mut s = window.get_stats();
                        s.cache_size = SharedString::from(&size);
                        window.set_stats(s);
                    }
                }
            }

        }
    });

    let config = load_config();
    let check_updates_on_start = config.check_updates_on_start;
    awaiting_appimage_check_init.set(check_updates_on_start && config.appimage_enabled);

    let _ = tx.send(UiMessage::SysInfoLoaded(load_sys_info()));
    let _ = tx.send(UiMessage::ActivityLoaded(load_recent_activity()));

    let tx_initial = tx.clone();
    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
        rt.block_on(async {
            let _ = tx_initial.send(UiMessage::SetLoading(true));
            load_packages_async(&tx_initial, check_updates_on_start).await;
        });
    });

    if check_updates_on_start && config.appimage_enabled {
        let tx_ai_start = tx.clone();
        thread::spawn(move || {
            let names = AppImageBackend::new()
                .map(|b| b.check_all_updates())
                .unwrap_or_default();
            let _ = tx_ai_start.send(UiMessage::AppImageUpdatesChecked(names));
        });
    }

    // On startup: re-add recorded user remotes that are missing (e.g. after a
    // flatpak reset), then write the full config back so it always reflects the
    // current state and materializes every field (incl. flatpak_remotes) on disk.
    thread::spawn(|| {
        let recorded = load_config().flatpak_remotes;
        let present: std::collections::HashSet<String> =
            list_user_flatpak_remotes().into_iter().map(|r| r.name).collect();
        for r in &recorded {
            if !r.name.is_empty() && !r.url.is_empty() && !present.contains(&r.name) {
                let _ = std::process::Command::new("flatpak")
                    .args(["remote-add", "--user", "--if-not-exists", &r.name, &r.url])
                    .status();
            }
        }
        let mut cfg = load_config();
        cfg.flatpak_remotes = list_user_flatpak_remotes();
        save_config(&cfg);
    });

    {
        let store_preload = flatpak_app_store.clone();
        let ids_preload = flatpak_installed_ids.clone();
        let loaded_preload = flatpak_loaded_remote.clone();
        let tx_preload = tx.clone();
        thread::spawn(move || {
            let remotes = fetch_flatpak_remotes();
            // Prefer flathub as the default view when present, else the first remote.
            let target = remotes.iter().find(|r| r.as_str() == "flathub").cloned()
                .or_else(|| remotes.first().cloned())
                .unwrap_or_else(|| "flathub".to_string());
            let _ = tx_preload.send(UiMessage::FlatpakRemotesLoaded(remotes));
            let (all_apps, installed) = load_remote_apps(&target);
            *ids_preload.lock().unwrap() = installed.clone();
            let all_pkg = apps_to_package_data(&all_apps, &installed, &target, "All", "");
            let total = all_pkg.len();
            let page: Vec<PackageData> = all_pkg.into_iter().take(FLATPAK_PAGE_SIZE).collect();
            *store_preload.lock().unwrap() = all_apps;
            *loaded_preload.lock().unwrap() = target;
            let _ = tx_preload.send(UiMessage::RemoteAppsFiltered { serial: u64::MAX, apps: page, total_matches: total });
        });
    }

    if let Some(ref path) = local_package_path
        && let Some(pkg_info) = get_local_package_info(path) {
            window.set_local_package(pkg_info);
            window.set_local_package_path(SharedString::from(path.as_str()));
            window.set_show_local_install(true);
            window.set_view(4);
        }

    window.on_refresh(move || {
        info!("Refresh requested");
        let tx = tx_load.clone();
        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            rt.block_on(async {
                let _ = tx.send(UiMessage::SetLoading(true));
                load_packages_async(&tx, false).await;
            });
        });
    });

    let store_rf = flatpak_app_store.clone();
    let loaded_rf = flatpak_loaded_remote.clone();
    let ids_rf = flatpak_installed_ids.clone();
    let tx_load_fp = tx.clone();
    let window_weak_rf = window.as_weak();
    window.on_refresh_flatpaks(move || {
        info!("Refresh flatpaks requested");
        let remote = window_weak_rf.upgrade()
            .map(|w| w.get_selected_remote().to_string())
            .unwrap_or_default();
        let store = store_rf.clone();
        let loaded = loaded_rf.clone();
        let ids = ids_rf.clone();
        let tx = tx_load_fp.clone();
        let weak = window_weak_rf.clone();
        if let Some(w) = weak.upgrade() { w.set_remote_apps_loading(true); }
        thread::spawn(move || {
            let target = if remote.is_empty() { "flathub".to_string() } else { remote };
            // Re-pull the remote's appstream (user installs need no root), drop the
            // browse cache, then re-parse fresh from disk.
            let _ = std::process::Command::new("flatpak")
                .args(["--user", "--noninteractive", "update", "--appstream", &target])
                .status();
            let _ = std::fs::remove_file(remote_cache_path(&target));
            let (all_apps, installed) = load_remote_apps(&target);
            *ids.lock().unwrap() = installed.clone();
            let all = apps_to_package_data(&all_apps, &installed, &target, "All", "");
            *store.lock().unwrap() = all_apps;
            *loaded.lock().unwrap() = target.clone();
            let total = all.len();
            let page: Vec<PackageData> = all.into_iter().take(FLATPAK_PAGE_SIZE).collect();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() { w.set_remote_apps_loading(false); }
            });
            // Reuse the standard delivery path for the grid.
            let _ = tx.send(UiMessage::RemoteAppsFiltered { serial: u64::MAX, apps: page, total_matches: total });
        });
    });

    let store_search = flatpak_app_store.clone();
    let ids_search = flatpak_installed_ids.clone();
    window.on_search(move |query| {
        info!("Search: {}", query);
        let tx = tx_search.clone();
        let query = query.to_string();
        let store = store_search.clone();
        let ids = ids_search.clone();
        thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
            rt.block_on(async {
                let _ = tx.send(UiMessage::SetLoading(true));
                search_packages_async(&tx, &query, store, ids).await;
            });
        });
    });

    let full_installed_page = full_installed.clone();
    let window_weak_lp = window.as_weak();
    window.on_load_page(move |page| {
        if let Some(window) = window_weak_lp.upgrade() {
            let ps = page_size as usize;
            let start = page as usize * ps;
            if window.get_view() == 0 {
                let data = full_installed_page.borrow();
                let page_data: Vec<PackageData> = data.iter().skip(start).take(ps).cloned().collect();
                let total = data.len().div_ceil(ps).max(1) as i32;
                window.set_installed_packages(ModelRc::new(VecModel::from(page_data)));
                window.set_total_pages(total);
            }
        }
    });

    let full_installed_filter = full_installed.clone();
    let window_weak_fi = window.as_weak();
    window.on_filter_installed(move |query| {
        if let Some(w) = window_weak_fi.upgrade() {
            let q = query.to_string().to_lowercase();
            let data = full_installed_filter.borrow();
            let filtered: Vec<PackageData> = if q.is_empty() {
                let ps = 50usize;
                let total = data.len().div_ceil(ps).max(1) as i32;
                w.set_total_pages(total);
                w.set_current_page(0);
                data.iter().take(ps).cloned().collect()
            } else {
                let filtered: Vec<PackageData> = data.iter().filter(|p| {
                    p.name.to_lowercase().contains(&q)
                        || p.display_name.to_lowercase().contains(&q)
                }).cloned().collect();
                w.set_total_pages(1);
                w.set_current_page(0);
                filtered
            };
            w.set_installed_packages(ModelRc::new(VecModel::from(filtered)));
        }
    });

    let full_fk_filter = full_installed_flatpaks.clone();
    let window_weak_fif = window.as_weak();
    window.on_filter_installed_flatpaks(move |query| {
        if let Some(w) = window_weak_fif.upgrade() {
            let q = query.to_string().to_lowercase();
            let data = full_fk_filter.borrow();
            let filtered: Vec<PackageData> = if q.is_empty() {
                data.clone()
            } else {
                data.iter().filter(|p| {
                    p.name.to_lowercase().contains(&q)
                        || p.display_name.to_lowercase().contains(&q)
                }).cloned().collect()
            };
            w.set_installed_flatpaks(ModelRc::new(VecModel::from(filtered)));
        }
    });

    let tx_ulk = tx.clone();
    let ulk_input = terminal_input_sender.clone();
    let ulk_pid = terminal_child_pid.clone();
    window.on_unlock_db(move || {
        info!("Unlock pacman DB");
        let tx = tx_ulk.clone();
        let input = ulk_input.clone();
        let pid = ulk_pid.clone();
        thread::spawn(move || {
            let script = "if [ -f /var/lib/pacman/db.lck ]; then \
                              rm -v /var/lib/pacman/db.lck && echo 'Lock file removed. Pacman DB unlocked.'; \
                          else \
                              echo 'No lock file found - DB is already unlocked.'; \
                          fi";
            run_in_terminal(&tx, "Unlocking Pacman Database", "pkexec", &["bash", "-c", script], &input, &pid);
        });
    });

    let window_weak_igr = window.as_weak();
    window.on_read_ignorepkg(move || {
        if let Some(w) = window_weak_igr.upgrade() {
            let content = std::fs::read_to_string("/etc/pacman.conf").unwrap_or_default();
            let mut active = false;
            let mut value = String::new();
            for line in content.lines() {
                let trimmed = line.trim();
                let stripped = trimmed.strip_prefix('#').unwrap_or(trimmed).trim();
                if let Some(rest) = stripped.strip_prefix("IgnorePkg") {
                    let v = rest.trim_start_matches([' ', '=']).trim().to_string();
                    if !trimmed.starts_with('#') {
                        active = true;
                    }
                    if !v.is_empty() {
                        value = v;
                    }
                    break;
                }
            }
            w.set_ignorepkg_active(active);
            w.set_ignorepkg_value(SharedString::from(value.as_str()));
            w.set_ignorepkg_edit_text(SharedString::from(w.get_ignorepkg_value().as_str()));
        }
    });

    window.on_save_ignorepkg(move |active, value| {
        let value = value.to_string();
        thread::spawn(move || {
            let line = if active {
                format!("IgnorePkg = {}", value.trim())
            } else {
                format!("#IgnorePkg = {}", value.trim())
            };
            let script = format!(
                "grep -q 'IgnorePkg' /etc/pacman.conf \
                 && sed -i 's|^#*[[:space:]]*IgnorePkg.*|{}|' /etc/pacman.conf \
                 || echo '{}' >> /etc/pacman.conf",
                line, line
            );
            let _ = std::process::Command::new("pkexec")
                .args(["bash", "-c", &script])
                .status();
        });
    });

    let window_weak_hpr = window.as_weak();
    window.on_read_holdpkg(move || {
        if let Some(w) = window_weak_hpr.upgrade() {
            let content = std::fs::read_to_string("/etc/pacman.conf").unwrap_or_default();
            let mut active = false;
            let mut value = String::new();
            for line in content.lines() {
                let trimmed = line.trim();
                let stripped = trimmed.strip_prefix('#').unwrap_or(trimmed).trim();
                if let Some(rest) = stripped.strip_prefix("HoldPkg") {
                    let v = rest.trim_start_matches([' ', '=']).trim().to_string();
                    if !trimmed.starts_with('#') { active = true; }
                    if !v.is_empty() { value = v; }
                    break;
                }
            }
            w.set_holdpkg_active(active);
            w.set_holdpkg_value(SharedString::from(value.as_str()));
            w.set_holdpkg_edit_text(SharedString::from(w.get_holdpkg_value().as_str()));
        }
    });

    window.on_save_holdpkg(move |active, value| {
        let value = value.to_string();
        thread::spawn(move || {
            let line = if active {
                format!("HoldPkg = {}", value.trim())
            } else {
                format!("#HoldPkg = {}", value.trim())
            };
            let script = format!(
                "grep -q 'HoldPkg' /etc/pacman.conf \
                 && sed -i 's|^#*[[:space:]]*HoldPkg.*|{}|' /etc/pacman.conf \
                 || echo '{}' >> /etc/pacman.conf",
                line, line
            );
            let _ = std::process::Command::new("pkexec")
                .args(["bash", "-c", &script])
                .status();
        });
    });

    let window_weak_fr = window.as_weak();
    window.on_load_flatpak_remotes(move || {
        if let Some(w) = window_weak_fr.upgrade() {
            w.set_flatpak_mgr_remotes_loading(true);
            let weak = w.as_weak();
            thread::spawn(move || {
                // Query each installation separately (so we know system vs user) and
                // include disabled remotes (--show-disabled). The `options` column
                // carries the "disabled" flag when a remote is turned off.
                let query = |scope: &str| -> Vec<FlatpakRemote> {
                    let system = scope == "--system";
                    std::process::Command::new("flatpak")
                        .args([scope, "remote-list", "--show-disabled", "--columns=name,url,options"])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                        .unwrap_or_default()
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(|l| {
                            let mut parts = l.split('\t');
                            let name = parts.next().unwrap_or("").trim().to_string();
                            let url = parts.next().unwrap_or("").trim().to_string();
                            let options = parts.next().unwrap_or("").trim().to_lowercase();
                            FlatpakRemote {
                                name: SharedString::from(name.as_str()),
                                url: SharedString::from(url.as_str()),
                                enabled: !options.split(',').any(|o| o.trim() == "disabled"),
                                system,
                            }
                        })
                        .collect()
                };
                let mut remotes = query("--system");
                remotes.extend(query("--user"));
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.set_flatpak_mgr_remotes(ModelRc::new(VecModel::from(remotes)));
                        w.set_flatpak_mgr_remotes_loading(false);
                    }
                });
            });
        }
    });

    let tx_add_remote = tx.clone();
    window.on_add_flatpak_remote({
        let window_weak_afr = window.as_weak();
        move |name, url| {
            let name = name.to_string();
            let url = url.to_string();
            let weak = window_weak_afr.clone();
            let tx = tx_add_remote.clone();
            thread::spawn(move || {
                // Add as a USER remote: no root prompt and it appears instantly.
                let ok = std::process::Command::new("flatpak")
                    .args(["remote-add", "--user", "--if-not-exists", &name, &url])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                // Refresh the modal list + browse dropdown immediately so the new
                // remote shows without closing/reopening the dialog.
                let _ = tx.send(UiMessage::FlatpakRemotesLoaded(fetch_flatpak_remotes()));
                {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(w) = weak.upgrade() { w.invoke_load_flatpak_remotes(); }
                    });
                }
                if ok {
                    // Record it in xpm's own settings file.
                    save_xpm_remotes();
                    // Pull this remote's appstream so it has browsable content, then
                    // drop any stale browse cache so the next view re-parses it.
                    let _ = std::process::Command::new("flatpak")
                        .args(["--user", "--noninteractive", "update", "--appstream", &name])
                        .status();
                    let _ = std::fs::remove_file(remote_cache_path(&name));
                }
            });
        }
    });

    window.on_remove_flatpak_remote({
        let window_weak_rfr = window.as_weak();
        let tx_rm = tx.clone();
        move |name, system| {
            let name = name.to_string();
            if name.eq_ignore_ascii_case("flathub") {
                return;
            }
            let weak = window_weak_rfr.clone();
            let tx = tx_rm.clone();
            thread::spawn(move || {
                let scope = if system { "--system" } else { "--user" };
                // System remotes need privilege escalation; user remotes do not.
                let _ = if system {
                    std::process::Command::new("pkexec")
                        .args(["flatpak", scope, "remote-delete", "--force", &name])
                        .status()
                } else {
                    std::process::Command::new("flatpak")
                        .args([scope, "remote-delete", "--force", &name])
                        .status()
                };
                let _ = std::fs::remove_file(remote_cache_path(&name));
                save_xpm_remotes();
                let remotes = fetch_flatpak_remotes();
                let _ = tx.send(UiMessage::FlatpakRemotesLoaded(remotes.clone()));
                let removed = name.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.invoke_load_flatpak_remotes();
                        switch_remote_if_gone(&w, &removed, &remotes);
                    }
                });
            });
        }
    });

    window.on_set_flatpak_remote_enabled({
        let window_weak_sre = window.as_weak();
        let tx_sre = tx.clone();
        move |name, system, enable| {
            let name = name.to_string();
            let weak = window_weak_sre.clone();
            let tx = tx_sre.clone();
            thread::spawn(move || {
                let scope = if system { "--system" } else { "--user" };
                let flag = if enable { "--enable" } else { "--disable" };
                // System remotes need privilege escalation; user remotes do not.
                let status = if system {
                    std::process::Command::new("pkexec")
                        .args(["flatpak", scope, "remote-modify", flag, &name])
                        .status()
                } else {
                    std::process::Command::new("flatpak")
                        .args([scope, "remote-modify", flag, &name])
                        .status()
                };
                let _ = status;
                // Browse dropdown lists only enabled remotes, so a disable should
                // drop it from there too.
                let remotes = fetch_flatpak_remotes();
                let _ = tx.send(UiMessage::FlatpakRemotesLoaded(remotes.clone()));
                let toggled = name.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.invoke_load_flatpak_remotes();
                        // fetch_flatpak_remotes returns only enabled remotes; if the
                        // selected one was just disabled it won't be present.
                        switch_remote_if_gone(&w, &toggled, &remotes);
                    }
                });
            });
        }
    });

    window.on_set_parallel_downloads(move |n| {
        let val = n as u32;
        thread::spawn(move || {
            let script = format!(
                "grep -q 'ParallelDownloads' /etc/pacman.conf \
                 && sed -i 's/^#*[[:space:]]*ParallelDownloads.*/ParallelDownloads = {}/' /etc/pacman.conf \
                 || echo 'ParallelDownloads = {}' >> /etc/pacman.conf",
                val, val
            );
            let _ = std::process::Command::new("pkexec")
                .args(["bash", "-c", &script])
                .status();
        });
    });


    window.on_load_repo_list({
        let tx = tx.clone();
        move || {
            let tx = tx.clone();
            thread::spawn(move || {
                let repos = std::fs::read_to_string("/etc/pacman.conf")
                    .map(|c| parse_pacman_repos(&c))
                    .unwrap_or_default();
                let _ = tx.send(UiMessage::RepoListLoaded(repos));
            });
        }
    });

    window.on_repo_mgr_toggle({
        let window_weak = window.as_weak();
        move |idx| {
            if let Some(w) = window_weak.upgrade() {
                let mut enabled: Vec<bool> = w.get_repo_mgr_enabled().iter().collect();
                let i = idx as usize;
                if i < enabled.len() {
                    enabled[i] = !enabled[i];
                }
                w.set_repo_mgr_enabled(ModelRc::new(VecModel::from(enabled)));
            }
        }
    });

    window.on_repo_apply_changes({
        let tx = tx.clone();
        let window_weak = window.as_weak();
        move || {
            let window = window_weak.unwrap();
            let list: Vec<RepoEntry> = window.get_repo_mgr_list().iter().collect();
            let enabled: Vec<bool> = window.get_repo_mgr_enabled().iter().collect();
            let tx = tx.clone();
            thread::spawn(move || {
                let content = match std::fs::read_to_string("/etc/pacman.conf") {
                    Ok(c) => c,
                    Err(_) => {
                        let _ = tx.send(UiMessage::SetStatus("Failed to read /etc/pacman.conf".to_string()));
                        return;
                    }
                };
                let mut new_content = content;
                for (repo, &en) in list.iter().zip(enabled.iter()) {
                    new_content = toggle_repo_in_conf(&new_content, &repo.name, en);
                }
                let ok = write_pacman_conf(&new_content);
                let msg = if ok {
                    "Repo changes saved successfully"
                } else {
                    "Failed to save repo changes (authentication cancelled?)"
                };
                let _ = tx.send(UiMessage::SetStatus(msg.to_string()));
            });
        }
    });

    window.on_repo_remove({
        let tx = tx.clone();
        move |name| {
            let name = name.to_string();
            let tx = tx.clone();
            thread::spawn(move || {
                let content = match std::fs::read_to_string("/etc/pacman.conf") {
                    Ok(c) => c,
                    Err(_) => {
                        let _ = tx.send(UiMessage::SetStatus("Failed to read /etc/pacman.conf".to_string()));
                        return;
                    }
                };
                let new_content = remove_repo_from_conf(&content, &name);
                let ok = write_pacman_conf(&new_content);
                if ok {
                    let repos = parse_pacman_repos(&new_content);
                    let _ = tx.send(UiMessage::RepoListLoaded(repos));
                    let _ = tx.send(UiMessage::SetStatus(format!("Repo '{}' removed", name)));
                } else {
                    let _ = tx.send(UiMessage::SetStatus("Failed to remove repo (authentication cancelled?)".to_string()));
                }
            });
        }
    });

    window.on_repo_add({
        let tx = tx.clone();
        move |name, server, siglevel| {
            let name = name.to_string();
            let server = server.to_string();
            let siglevel = siglevel.to_string();
            let tx = tx.clone();
            thread::spawn(move || {
                let content = match std::fs::read_to_string("/etc/pacman.conf") {
                    Ok(c) => c,
                    Err(_) => {
                        let _ = tx.send(UiMessage::SetStatus("Failed to read /etc/pacman.conf".to_string()));
                        return;
                    }
                };
                let new_content = add_repo_to_conf(&content, &name, &server, &siglevel);
                let ok = write_pacman_conf(&new_content);
                if ok {
                    let repos = parse_pacman_repos(&new_content);
                    let _ = tx.send(UiMessage::RepoListLoaded(repos));
                    let _ = tx.send(UiMessage::SetStatus(format!("Repo '{}' added", name)));
                } else {
                    let _ = tx.send(UiMessage::SetStatus("Failed to add repo (authentication cancelled?)".to_string()));
                }
            });
        }
    });

    window.on_open_pacman_conf_editor(move || {
        let _ = std::process::Command::new("bash")
            .arg("-c")
            .arg("konsole -e sudo nano /etc/pacman.conf 2>/dev/null \
                  || xterm -e 'sudo nano /etc/pacman.conf' 2>/dev/null \
                  || alacritty -e sudo nano /etc/pacman.conf 2>/dev/null \
                  || foot -e sudo nano /etc/pacman.conf 2>/dev/null \
                  || kitty sudo nano /etc/pacman.conf 2>/dev/null \
                  || gnome-terminal -- sudo nano /etc/pacman.conf 2>/dev/null")
            .spawn();
    });

    let tx_load_opts = tx.clone();
    window.on_load_pacman_opts(move || {
        let tx = tx_load_opts.clone();
        thread::spawn(move || {
            let content = std::fs::read_to_string("/etc/pacman.conf").unwrap_or_default();
            let opts = parse_pacman_opts(&content);
            let _ = tx.send(UiMessage::PacmanOptsLoaded(opts));
        });
    });

    let tx_save_opts = tx.clone();
    let win_save_opts = window.as_weak();
    window.on_save_pacman_opts(move || {
        let tx = tx_save_opts.clone();
        let window = win_save_opts.upgrade().unwrap();
        let opts = PacmanOpts {
            color: window.get_opt_color(),
            love_candy: window.get_opt_love_candy(),
            verbose_pkg_lists: window.get_opt_verbose_pkg_lists(),
            disable_dl_timeout: window.get_opt_disable_dl_timeout(),
            check_space: window.get_opt_check_space(),
            disable_sandbox: window.get_opt_disable_sandbox(),
            no_progress_bar: window.get_opt_no_progress_bar(),
            use_syslog: window.get_opt_use_syslog(),
            clean_method: window.get_opt_clean_method(),
        };
        thread::spawn(move || {
            let content = match std::fs::read_to_string("/etc/pacman.conf") {
                Ok(c) => c,
                Err(_) => {
                    let _ = tx.send(UiMessage::SetStatus("Failed to read /etc/pacman.conf".to_string()));
                    return;
                }
            };
            let new_content = write_pacman_opts(&content, &opts);
            if write_pacman_conf(&new_content) {
                let _ = tx.send(UiMessage::SetStatus("pacman.conf options saved".to_string()));
                let reloaded = parse_pacman_opts(&new_content);
                let _ = tx.send(UiMessage::PacmanOptsLoaded(reloaded));
            } else {
                let _ = tx.send(UiMessage::SetStatus("Failed to save options (authentication cancelled?)".to_string()));
            }
        });
    });


    let tx_install = tx.clone();
    let install_input = terminal_input_sender.clone();
    let install_pid = terminal_child_pid.clone();
    let install_ctx = conflict_context.clone();
    window.on_install_package(move |name, backend| {
        info!("Install: {} (backend: {})", name, backend);
        let tx = tx_install.clone();
        let name = name.to_string();
        let input = install_input.clone();
        let pid = install_pid.clone();
        let ctx = install_ctx.clone();
        thread::spawn(move || {
            let title = format!("Installing {}", name);
            run_managed_operation(&tx, &title, "install", &[name], backend, &input, &pid, &ctx);
        });
    });

    let tx_remove = tx.clone();
    let remove_input = terminal_input_sender.clone();
    let remove_pid = terminal_child_pid.clone();
    let remove_ctx = conflict_context.clone();
    window.on_remove_package(move |name, backend| {
        info!("Remove: {} (backend: {})", name, backend);
        let tx = tx_remove.clone();
        let name = name.to_string();
        let input = remove_input.clone();
        let pid = remove_pid.clone();
        let ctx = remove_ctx.clone();
        thread::spawn(move || {
            let title = format!("Removing {}", name);
            run_managed_operation(&tx, &title, "remove", &[name], backend, &input, &pid, &ctx);
        });
    });

    let tx_upd = tx.clone();
    let upd_input = terminal_input_sender.clone();
    let upd_pid = terminal_child_pid.clone();
    let upd_ctx = conflict_context.clone();
    window.on_update_package(move |name, backend| {
        info!("Update: {} (backend: {})", name, backend);
        let tx = tx_upd.clone();
        let name = name.to_string();
        let input = upd_input.clone();
        let pid = upd_pid.clone();
        let ctx = upd_ctx.clone();
        thread::spawn(move || {
            let title = format!("Updating {}", name);
            run_managed_operation(&tx, &title, "update", &[name], backend, &input, &pid, &ctx);
        });
    });

    let tx_update = tx.clone();
    let update_all_input = terminal_input_sender.clone();
    let update_all_pid = terminal_child_pid.clone();
    let update_all_appimage_dir = appimage_dir_state.clone();
    let update_all_appimage_pending = appimage_updates.clone();
    let window_weak_ua = window.as_weak();
    window.on_update_all(move || {
        let Some(w) = window_weak_ua.upgrade() else { return };
        let native = w.get_update_packages().row_count() > 0;
        let flatpak = w.get_setting_flatpak_enabled() && w.get_flatpak_update_packages().row_count() > 0;
        let appimages: Vec<String> = if w.get_setting_appimage_enabled() {
            update_all_appimage_pending.borrow().iter().cloned().collect()
        } else {
            Vec::new()
        };
        if !native && !flatpak && appimages.is_empty() {
            return;
        }
        info!("Update all: native={native} flatpak={flatpak} appimages={}", appimages.len());
        let needs_reboot = native_updates_need_reboot(&w);
        let plan = UpdatePlan {
            native,
            flatpak,
            appimages,
            appimage_dir: update_all_appimage_dir.lock().unwrap().clone(),
        };
        let tx = tx_update.clone();
        let input = update_all_input.clone();
        let pid = update_all_pid.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetTerminalIsUpgrade(needs_reboot));
            run_update_all(&tx, &input, &pid, plan);
        });
    });

    let tx_arch_news = tx.clone();
    window.on_refresh_arch_news(move || {
        let tx = tx_arch_news.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::ArchNewsLoading);
            let items = fetch_arch_news();
            let _ = tx.send(UiMessage::ArchNewsLoaded(items));
        });
    });

    let tx_req_install = tx.clone();
    let req_install_input = terminal_input_sender.clone();
    let req_install_pid = terminal_child_pid.clone();
    let req_install_ctx = conflict_context.clone();
    window.on_request_install(move |name, backend| {
        let tx = tx_req_install.clone();
        let n = name.to_string();
        let input = req_install_input.clone();
        let pid = req_install_pid.clone();
        let ctx = req_install_ctx.clone();
        thread::spawn(move || {
            let title = format!("Installing {}", n);
            run_managed_operation(&tx, &title, "install", &[n], backend, &input, &pid, &ctx);
        });
    });

    let tx_req_remove = tx.clone();
    let req_remove_input = terminal_input_sender.clone();
    let req_remove_pid = terminal_child_pid.clone();
    let req_remove_ctx = conflict_context.clone();
    window.on_request_remove(move |name, backend| {
        let tx = tx_req_remove.clone();
        let n = name.to_string();
        let input = req_remove_input.clone();
        let pid = req_remove_pid.clone();
        let ctx = req_remove_ctx.clone();
        thread::spawn(move || {
            let title = format!("Removing {}", n);
            run_managed_operation(&tx, &title, "remove", &[n], backend, &input, &pid, &ctx);
        });
    });

    let tx_dep_install = tx.clone();
    let dep_install_input = terminal_input_sender.clone();
    let dep_install_pid = terminal_child_pid.clone();
    let dep_install_ctx = conflict_context.clone();
    window.on_install_dep_package(move |name| {
        let tx = tx_dep_install.clone();
        let n = name.to_string();
        let input = dep_install_input.clone();
        let pid = dep_install_pid.clone();
        let ctx = dep_install_ctx.clone();
        thread::spawn(move || {
            let title = format!("Installing dependency: {}", n);
            run_managed_operation(&tx, &title, "install", &[n], 0, &input, &pid, &ctx);
        });
    });

    let tx_fp_remove = tx.clone();
    let fp_remove_input = terminal_input_sender.clone();
    let fp_remove_pid = terminal_child_pid.clone();
    let fp_remove_ctx = conflict_context.clone();
    window.on_remove_flatpak(move |app_id, also_delete_data| {
        let tx = tx_fp_remove.clone();
        let id = app_id.to_string();
        let input = fp_remove_input.clone();
        let pid = fp_remove_pid.clone();
        let ctx = fp_remove_ctx.clone();
        thread::spawn(move || {
            *ctx.lock().unwrap() = Some(("remove".to_string(), vec![id.clone()], 1));
            let title = format!("Removing {}", id);
            let mut args = vec!["uninstall", "--assumeyes", &id];
            if also_delete_data { args.push("--delete-data"); }
            run_in_terminal(&tx, &title, "flatpak", &args, &input, &pid);
        });
    });

    let tx_req_update = tx.clone();
    let req_update_input = terminal_input_sender.clone();
    let req_update_pid = terminal_child_pid.clone();
    let req_update_ctx = conflict_context.clone();
    window.on_request_update(move |name, backend| {
        let tx = tx_req_update.clone();
        let n = name.to_string();
        let input = req_update_input.clone();
        let pid = req_update_pid.clone();
        let ctx = req_update_ctx.clone();
        thread::spawn(move || {
            let title = format!("Updating {}", n);
            run_managed_operation(&tx, &title, "update", &[n], backend, &input, &pid, &ctx);
        });
    });


    let window_weak_cp = window.as_weak();
    window.on_close_progress_popup(move || {
        if let Some(window) = window_weak_cp.upgrade() {
            window.set_show_progress_popup(false);
        }
    });

    let cancel_pid = terminal_child_pid.clone();
    let cancel_input = terminal_input_sender.clone();
    let tx_cancel = tx.clone();
    window.on_cancel_operation(move || {
        info!("Operation cancelled by user");
        progress::request_cancel();
        let _ = tx_cancel.send(UiMessage::ProgressStage("Cancelling...".to_string()));
        if let Some(pid) = *cancel_pid.lock().unwrap() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
        }
        *cancel_input.lock().unwrap() = None;
    });

    let progress_input = terminal_input_sender.clone();
    let window_weak_pp = window.as_weak();
    window.on_progress_popup_send_input(move |text| {
        let text_str = text.to_string();
        if let Some(sender) = progress_input.lock().unwrap().as_ref() {
            let _ = sender.send(text_str);
        }
        if let Some(window) = window_weak_pp.upgrade() {
            window.set_progress_popup_show_input(false);
            window.set_progress_popup_prompt(SharedString::from(""));
        }
    });

    let tx_proceed = tx.clone();
    let input_proceed = terminal_input_sender.clone();
    let window_weak_proceed = window.as_weak();
    window.on_progress_popup_proceed(move || {
        if let Some(sender) = input_proceed.lock().unwrap().as_ref() {
            let _ = sender.send("y".to_string());
        }
        if let Some(window) = window_weak_proceed.upgrade() {
            window.set_progress_popup_show_input(false);
            window.set_progress_popup_show_buttons(false);
            window.set_progress_popup_prompt(SharedString::from(""));
        }
        let _ = tx_proceed.send(UiMessage::ProgressHidePrompt);
    });

    let selected_pkgs_toggle = selected_packages.clone();
    let window_weak_tps = window.as_weak();
    window.on_toggle_package_selected(move |name, backend, selected| {
        let name_str = name.to_string();
        let mut sel = selected_pkgs_toggle.borrow_mut();

        if let Some(window) = window_weak_tps.upgrade() {
            let is_installed = find_package_installed(&window, &name_str, backend);

            if selected {
                if !sel.iter().any(|(n, b, _)| n == &name_str && *b == backend) {
                    sel.push((name_str.clone(), backend, is_installed));
                }
            } else {
                sel.retain(|(n, b, _)| !(n == &name_str && *b == backend));
            }

            window.set_selected_count(sel.len() as i32);
            let installed_count = sel.iter().filter(|(_, _, inst)| *inst).count() as i32;
            window.set_selected_installed_count(installed_count);
            window.set_selected_uninstalled_count(sel.len() as i32 - installed_count);
            update_selection_in_models(&window, &name_str, backend, selected);
        }
    });

    let selected_pkgs_clear = selected_packages.clone();
    let window_weak_cs = window.as_weak();
    window.on_clear_selection(move || {
        let mut sel = selected_pkgs_clear.borrow_mut();
        let old_sel: Vec<(String, i32, bool)> = sel.drain(..).collect();
        if let Some(window) = window_weak_cs.upgrade() {
            window.set_selected_count(0);
            window.set_selected_installed_count(0);
            window.set_selected_uninstalled_count(0);
            for (name, backend, _) in &old_sel {
                update_selection_in_models(&window, name, *backend, false);
            }
        }
    });

    let selected_pkgs_bi = selected_packages.clone();
    let tx_bulk_install = tx.clone();
    let bulk_install_input = terminal_input_sender.clone();
    let bulk_install_pid = terminal_child_pid.clone();
    let bulk_install_ctx = conflict_context.clone();
    window.on_bulk_install(move || {
        let sel = selected_pkgs_bi.borrow();
        let uninstalled: Vec<&(String, i32, bool)> = sel.iter().filter(|(_, _, inst)| !inst).collect();
        if uninstalled.is_empty() { return; }
        let names: Vec<String> = uninstalled.iter().map(|(n, _, _)| n.clone()).collect();
        let backend = uninstalled[0].1;
        let tx = tx_bulk_install.clone();
        let input = bulk_install_input.clone();
        let pid = bulk_install_pid.clone();
        let ctx = bulk_install_ctx.clone();
        let title = format!("Installing {} packages", names.len());
        thread::spawn(move || {
            run_managed_operation(&tx, &title, "install", &names, backend, &input, &pid, &ctx);
        });
    });

    let selected_pkgs_br = selected_packages.clone();
    let tx_bulk_remove = tx.clone();
    let bulk_remove_input = terminal_input_sender.clone();
    let bulk_remove_pid = terminal_child_pid.clone();
    let bulk_remove_ctx = conflict_context.clone();
    window.on_bulk_remove(move || {
        let sel = selected_pkgs_br.borrow();
        let installed: Vec<&(String, i32, bool)> = sel.iter().filter(|(_, _, inst)| *inst).collect();
        if installed.is_empty() { return; }
        let names: Vec<String> = installed.iter().map(|(n, _, _)| n.clone()).collect();
        let backend = installed[0].1;
        let tx = tx_bulk_remove.clone();
        let input = bulk_remove_input.clone();
        let pid = bulk_remove_pid.clone();
        let ctx = bulk_remove_ctx.clone();
        let title = format!("Removing {} packages", names.len());
        thread::spawn(move || {
            run_managed_operation(&tx, &title, "remove", &names, backend, &input, &pid, &ctx);
        });
    });

    let tx_clean = tx.clone();
    let clean_input = terminal_input_sender.clone();
    let clean_pid = terminal_child_pid.clone();
    window.on_clean_package_cache(move || {
        info!("Clean package cache");
        let tx = tx_clean.clone();
        let input = clean_input.clone();
        let pid = clean_pid.clone();
        thread::spawn(move || {
            let script = "rm -rf /var/cache/pacman/pkg/download-* 2>/dev/null; \
                          yes 2>/dev/null | LANG=C pacman -Scc; \
                          echo 'Done.'";
            run_in_terminal(&tx, "Cleaning Package Cache", "pkexec", &["bash", "-c", script], &input, &pid);
            let bytes = std::process::Command::new("du")
                .args(["-sb", "/var/cache/pacman/pkg"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next()
                    .and_then(|s| s.parse::<u64>().ok()))
                .unwrap_or(0);
            let _ = tx.send(UiMessage::UpdateCacheSize(format_size(bytes)));
        });
    });

    window.on_clean_app_cache(move || {
        info!("Clean app cache and restart");
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        // The AppImage store (installed apps + manifest) lives in the same directory: only drop caches.
        if let Ok(entries) = std::fs::read_dir(format!("{}/.local/share/xpm", home)) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == "pkg_cache.json" || (name.starts_with("remote_") && name.ends_with(".json")) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            let _ = std::process::Command::new(exe).spawn();
        }
        std::process::exit(0);
    });

    let window_weak_ot = window.as_weak();
    window.on_orphan_toggle(move |idx| {
        if let Some(w) = window_weak_ot.upgrade() {
            let mut checked: Vec<bool> = w.get_orphan_checked().iter().collect();
            let i = idx as usize;
            if i < checked.len() {
                checked[i] = !checked[i];
                let count = checked.iter().filter(|&&c| c).count() as i32;
                w.set_orphan_checked(ModelRc::new(VecModel::from(checked)));
                w.set_orphan_selected_count(count);
            }
        }
    });

    let window_weak_osa = window.as_weak();
    window.on_orphan_select_all(move |select| {
        if let Some(w) = window_weak_osa.upgrade() {
            let len = w.get_orphan_list().row_count();
            let checked = vec![select; len];
            let count = if select { len as i32 } else { 0 };
            w.set_orphan_checked(ModelRc::new(VecModel::from(checked)));
            w.set_orphan_selected_count(count);
        }
    });

    let window_weak_odi = window.as_weak();
    window.on_load_orphan_dep_info(move |pkg_name| {
        let name = pkg_name.to_string();
        let window_weak = window_weak_odi.clone();
        thread::spawn(move || {
            let qi = std::process::Command::new("pacman")
                .args(["-Qi", &name])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();

            let required_by = qi.lines()
                .find(|l| l.starts_with("Required By"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();

            let optional_for = qi.lines()
                .find(|l| l.starts_with("Optional For"))
                .and_then(|l| l.split(':').nth(1))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();

            let info = match (required_by.is_empty() || required_by == "None",
                              optional_for.is_empty() || optional_for == "None") {
                (true, true)   => "Not required by any installed package.".to_string(),
                (false, true)  => format!("Required by: {}", required_by),
                (true, false)  => format!("Optional for: {}", optional_for),
                (false, false) => format!("Required by: {}  |  Optional for: {}", required_by, optional_for),
            };

            let info_shared = SharedString::from(info.as_str());
            slint::invoke_from_event_loop(move || {
                if let Some(w) = window_weak.upgrade() {
                    w.set_orphan_dep_info_text(info_shared);
                }
            }).ok();
        });
    });

    let window_weak_orp = window.as_weak();
    let tx_orp_load = tx.clone();
    window.on_load_orphan_list(move || {
        let tx = tx_orp_load.clone();
        let window_weak = window_weak_orp.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetBusy(true));
            let output = std::process::Command::new("pacman")
                .args(["-Qdtq"])
                .output();
            let _ = tx.send(UiMessage::SetBusy(false));
            let names: Vec<String> = match output {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(|l| l.trim().to_string())
                        .filter(|l| !l.is_empty())
                        .collect()
                }
                _ => Vec::new(),
            };

            let pkgs: Vec<PackageData> = names.iter().map(|name| {
                let qi = std::process::Command::new("pacman")
                    .args(["-Qi", name])
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                    .unwrap_or_default();

                let mut desc = String::new();
                let mut version = String::new();
                let mut explicit = false;
                for line in qi.lines() {
                    if let Some(v) = line.strip_prefix("Description     : ") { desc = v.trim().to_string(); }
                    if let Some(v) = line.strip_prefix("Version         : ") { version = v.trim().to_string(); }
                    if let Some(v) = line.strip_prefix("Install Reason  : ") {
                        explicit = v.trim().contains("Explicitly");
                    }
                }

                PackageData {
                    name: SharedString::from(name.as_str()),
                    display_name: SharedString::from(name.as_str()),
                    description: SharedString::from(desc.as_str()),
                    version: SharedString::from(version.as_str()),
                    backend: 0,
                    installed: true,
                    explicit,
                    repository: SharedString::from("local"),
                    ..Default::default()
                }
            }).collect();

            let len = pkgs.len();
            let checked = vec![false; len];
            slint::invoke_from_event_loop(move || {
                if let Some(w) = window_weak.upgrade() {
                    w.set_orphan_list(ModelRc::new(VecModel::from(pkgs)));
                    w.set_orphan_checked(ModelRc::new(VecModel::from(checked)));
                    w.set_orphan_selected_count(0);
                }
            }).ok();
        });
    });

    let window_weak_orp_rm = window.as_weak();
    let tx_orphans = tx.clone();
    let orphan_input = terminal_input_sender.clone();
    let orphan_pid = terminal_child_pid.clone();
    window.on_remove_selected_orphans(move || {
        let Some(w) = window_weak_orp_rm.upgrade() else { return; };
        let pkgs: Vec<PackageData> = w.get_orphan_list().iter().collect();
        let checked: Vec<bool> = w.get_orphan_checked().iter().collect();
        let selected: Vec<String> = pkgs.iter().zip(checked.iter())
            .filter(|&(_, &c)| c)
            .map(|(p, _)| p.name.to_string())
            .collect();
        if selected.is_empty() { return; }
        let tx = tx_orphans.clone();
        let input = orphan_input.clone();
        let pid = orphan_pid.clone();
        thread::spawn(move || {
            let pkg_list = selected.join(" ");
            let script = format!("pacman -Rns {}", pkg_list);
            run_in_terminal(&tx, "Removing Orphan Packages", "pkexec",
                &["bash", "-c", &script], &input, &pid);
        });
    });

    let tx_orphans_legacy = tx.clone();
    let orphan_input_legacy = terminal_input_sender.clone();
    let orphan_pid_legacy = terminal_child_pid.clone();
    window.on_remove_orphans(move || {
        info!("Remove orphans (legacy)");
        let tx = tx_orphans_legacy.clone();
        let input = orphan_input_legacy.clone();
        let pid = orphan_pid_legacy.clone();
        thread::spawn(move || {
            run_in_terminal(&tx, "Removing Orphan Packages", "pkexec",
                &["bash", "-c", "pacman -Qdtq | pacman -Rns -"], &input, &pid);
        });
    });


    let tx_sync = tx.clone();
    let ai_enabled_sync = appimage_enabled_flag.clone();
    window.on_sync_databases(move || {
        info!("Check for updates");
        if ai_enabled_sync.load(std::sync::atomic::Ordering::Relaxed) {
            let tx_ai = tx_sync.clone();
            thread::spawn(move || {
                let names = AppImageBackend::new()
                    .map(|b| b.check_all_updates())
                    .unwrap_or_default();
                let _ = tx_ai.send(UiMessage::AppImageUpdatesChecked(names));
            });
        }
        let tx = tx_sync.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetBusy(true));
            let _ = tx.send(UiMessage::SetProgress(5));
            let _ = tx.send(UiMessage::SetProgressText("Syncing pacman databases...".to_string()));
            let _ = tx.send(UiMessage::SetStatus("Syncing pacman databases...".to_string()));

            let pacman_ok = match std::process::Command::new("pkexec")
            .args(["pacman", "-Syy"])
            .output()
            {
                Ok(r) if r.status.success() => {
                    let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText("Pacman synced. Checking Flatpak...".to_string()));
                    let _ = tx.send(UiMessage::SetStatus("Pacman synced. Checking Flatpak...".to_string()));
                    true
                }
                Ok(r) => {
                    let stderr = String::from_utf8_lossy(&r.stderr);
                    if stderr.contains("cancelled") || stderr.contains("dismissed")
                        || r.status.code() == Some(126) || r.status.code() == Some(127)
                        {
                            let _ = tx.send(UiMessage::SetStatus("Authentication cancelled".to_string()));
                            let _ = tx.send(UiMessage::SetProgress(0));
                            let _ = tx.send(UiMessage::SetProgressText("".to_string()));
                            let _ = tx.send(UiMessage::SetBusy(false));
                            return;
                        }
                        let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText("Pacman sync had issues, continuing...".to_string()));
                    let _ = tx.send(UiMessage::SetStatus("Pacman sync had issues, continuing...".to_string()));
                    false
                }
                Err(_) => {
                    let _ = tx.send(UiMessage::SetProgress(25));
                    let _ = tx.send(UiMessage::SetProgressText("Pacman sync unavailable, continuing...".to_string()));
                    let _ = tx.send(UiMessage::SetStatus("Pacman sync unavailable, continuing...".to_string()));
                    false
                }
            };

            let _ = tx.send(UiMessage::SetProgress(50));
            let _ = tx.send(UiMessage::SetProgressText("Refreshing Flatpak metadata...".to_string()));
            let _ = tx.send(UiMessage::SetStatus("Refreshing Flatpak metadata...".to_string()));
            let _flatpak_ok = match std::process::Command::new("flatpak")
            .args(["update", "--appstream", "-y"])
            .output()
            {
                Ok(r) => r.status.success(),
                      Err(_) => false,
            };

            let _ = tx.send(UiMessage::SetProgress(75));
            let _ = tx.send(UiMessage::SetProgressText("Reloading packages...".to_string()));
            let _ = tx.send(UiMessage::SetStatus("Checking for updates...".to_string()));
            let rt = tokio::runtime::Runtime::new().expect("Runtime");
            rt.block_on(async {
                let _ = tx.send(UiMessage::SetLoading(true));
                load_packages_async(&tx, true).await;
            });

            let _ = tx.send(UiMessage::SetProgress(100));
            let _ = tx.send(UiMessage::SetProgressText("Complete".to_string()));

            let status = if pacman_ok {
                "Update check complete".to_string()
            } else {
                "Update check complete (pacman sync had issues)".to_string()
            };
            let _ = tx.send(UiMessage::SetProgress(0));
            let _ = tx.send(UiMessage::SetProgressText("".to_string()));
            let _ = tx.send(UiMessage::SetBusy(false));
            let _ = tx.send(UiMessage::SetStatus(status));
        });
    });

    window.on_open_url(move |url| {
        info!("Open URL: {}", url);
        let _ = open::that(url.as_str());
    });

    let tx_local = tx.clone();
    let local_input = terminal_input_sender.clone();
    let local_pid = terminal_child_pid.clone();
    let window_weak_local = window.as_weak();
    window.on_install_local_package(move |path| {
        info!("Install local package: {}", path);
        let tx = tx_local.clone();
        let path = path.to_string();
        let input = local_input.clone();
        let pid = local_pid.clone();

        if let Some(window) = window_weak_local.upgrade() {
            window.set_show_local_install(false);
        }

        thread::spawn(move || {
            let title = format!("Installing {}", path);
            run_in_terminal(&tx, &title, "pkexec", &["pacman", "-U", &path], &input, &pid);
        });
    });

    let window_weak = window.as_weak();
    window.on_cancel_local_install(move || {
        info!("Cancelled local package install");
        if let Some(window) = window_weak.upgrade() {
            window.set_show_local_install(false);
            window.set_view(0);
        }
    });

    let tx_ai_load = tx.clone();
    window.on_load_installed_appimages(move || {
        let tx = tx_ai_load.clone();
        thread::spawn(move || {
            if let Ok(backend) = AppImageBackend::new() {
                let _ = tx.send(UiMessage::InstalledAppImagesLoaded(backend.list_entries()));
            }
        });
    });

    let (tx_fp_file, input_fp_file, pid_fp_file, ctx_fp_file) =
        (tx.clone(), terminal_input_sender.clone(), terminal_child_pid.clone(), conflict_context.clone());
    window.on_install_flatpak_file({
        let weak = window.as_weak();
        let (tx, input, pid, ctx) = (tx_fp_file.clone(), input_fp_file.clone(), pid_fp_file.clone(), ctx_fp_file.clone());
        move || {
            let (weak, tx, input, pid, ctx) = (weak.clone(), tx.clone(), input.clone(), pid.clone(), ctx.clone());
            thread::spawn(move || {
                let Some(path) = pick_flatpak_file() else { return };
                run_flatpak_file(&weak, &tx, &path, &input, &pid, &ctx);
            });
        }
    });
    window.on_open_flatpak_file({
        let weak = window.as_weak();
        move |path| {
            let (weak, tx, input, pid, ctx) =
                (weak.clone(), tx_fp_file.clone(), input_fp_file.clone(), pid_fp_file.clone(), ctx_fp_file.clone());
            let path = path.to_string();
            thread::spawn(move || run_flatpak_file(&weak, &tx, &path, &input, &pid, &ctx));
        }
    });
    if let Some(path) = flatpak_file_arg.as_deref() {
        window.invoke_open_flatpak_file(SharedString::from(path));
    }

    let tx_ai_file = tx.clone();
    let dir_ai_file = appimage_dir_state.clone();
    window.on_install_appimage_file(move || {
        let tx = tx_ai_file.clone();
        let dir = dir_ai_file.lock().unwrap().clone();
        thread::spawn(move || {
            let Some(path) = pick_appimage_file() else { return };
            let title = format!("Installing {}", path);
            run_appimage_op(&tx, &title, Some(dir), None, |backend, log| {
                backend.install(&path, log).map(|_| ())
            });
        });
    });

    let tx_ai_url = tx.clone();
    let dir_ai_url = appimage_dir_state.clone();
    window.on_install_appimage_url(move |url| {
        let url = url.trim().to_string();
        if url.is_empty() {
            return;
        }
        let tx = tx_ai_url.clone();
        let dir = dir_ai_url.lock().unwrap().clone();
        thread::spawn(move || {
            let title = format!("Installing {}", url);
            run_appimage_op(&tx, &title, Some(dir), None, |backend, log| {
                backend.install(&url, log).map(|_| ())
            });
        });
    });

    let tx_ai_remove = tx.clone();
    window.on_remove_appimage(move |name| {
        let name = name.to_string();
        let tx = tx_ai_remove.clone();
        thread::spawn(move || {
            let title = format!("Removing {}", name);
            run_appimage_op(&tx, &title, None, None, |backend, log| backend.remove_app(&name, log));
        });
    });

    let tx_ai_update = tx.clone();
    let dir_ai_update = appimage_dir_state.clone();
    window.on_update_appimage(move |name| {
        let name = name.to_string();
        let tx = tx_ai_update.clone();
        let dir = dir_ai_update.lock().unwrap().clone();
        thread::spawn(move || {
            let title = format!("Updating {}", name);
            run_appimage_op(&tx, &title, Some(dir), Some(name.clone()), |backend, log| {
                backend.update_app(&name, log).map(|_| ())
            });
        });
    });

    let tx_ai_reinstall = tx.clone();
    let dir_ai_reinstall = appimage_dir_state.clone();
    window.on_reinstall_appimage(move |name| {
        let name = name.to_string();
        let tx = tx_ai_reinstall.clone();
        let dir = dir_ai_reinstall.lock().unwrap().clone();
        thread::spawn(move || {
            let title = format!("Reinstalling {}", name);
            run_appimage_op(&tx, &title, Some(dir), Some(name.clone()), |backend, log| {
                backend.reinstall_app(&name, log).map(|_| ())
            });
        });
    });

    let tx_ai_cat = tx.clone();
    let cat_load = appimage_catalog.clone();
    let sources_load = appimage_sources_state.clone();
    window.on_load_appimage_catalog(move || {
        if !cat_load.lock().unwrap().is_empty() {
            return;
        }
        let tx = tx_ai_cat.clone();
        let cache = cat_load.clone();
        let named: Vec<(String, String)> =
            sources_load.lock().unwrap().iter().map(|f| (f.name.clone(), f.url.clone())).collect();
        let _ = tx.send(UiMessage::AppImageCatalogLoading(true));
        thread::spawn(move || {
            let entries = xpm_appimage::catalog::fetch_sources_named(&named);
            *cache.lock().unwrap() = entries;
            let _ = tx.send(UiMessage::AppImageCatalogReady);
        });
    });

    let tx_ai_reload = tx.clone();
    let cat_reload = appimage_catalog.clone();
    let sources_reload = appimage_sources_state.clone();
    window.on_reload_appimage_catalog(move || {
        let tx = tx_ai_reload.clone();
        let cache = cat_reload.clone();
        let named: Vec<(String, String)> =
            sources_reload.lock().unwrap().iter().map(|f| (f.name.clone(), f.url.clone())).collect();
        let urls: Vec<String> = named.iter().map(|(_, u)| u.clone()).collect();
        let _ = tx.send(UiMessage::AppImageCatalogLoading(true));
        thread::spawn(move || {
            xpm_appimage::catalog::clear_feed_cache(&urls);
            let entries = xpm_appimage::catalog::fetch_sources_named(&named);
            *cache.lock().unwrap() = entries;
            let _ = tx.send(UiMessage::AppImageCatalogReady);
        });
    });

    let cat_filter = appimage_catalog.clone();
    let entries_filter = appimage_entries.clone();
    let win_ai_filter = window.as_weak();
    window.on_filter_appimage_catalog(move |query, page| {
        if let Some(w) = win_ai_filter.upgrade() {
            let page = page.max(0) as usize;
            let (cards, total) = filter_catalog(
                &cat_filter.lock().unwrap(),
                query.as_str(),
                w.get_selected_appimage_source().as_str(),
                &installed_github_map(&entries_filter.borrow()),
                page,
            );
            w.set_appimage_catalog_total(total as i32);
            w.set_appimage_page(clamp_appimage_page(page, total) as i32);
            w.set_catalog_appimages(ModelRc::new(VecModel::from(cards)));
        }
    });

    // Switch which AppImage source the catalog shows (mirrors flatpak remotes).
    let cat_src = appimage_catalog.clone();
    let entries_src = appimage_entries.clone();
    let win_ai_src = window.as_weak();
    window.on_browse_appimage_source(move |source| {
        if let Some(w) = win_ai_src.upgrade() {
            w.set_selected_appimage_source(source.clone());
            w.set_appimage_page(0);
            let (cards, total) = filter_catalog(
                &cat_src.lock().unwrap(),
                w.get_appimage_search().as_str(),
                source.as_str(),
                &installed_github_map(&entries_src.borrow()),
                0,
            );
            w.set_appimage_catalog_total(total as i32);
            w.set_catalog_appimages(ModelRc::new(VecModel::from(cards)));
        }
    });

    let tx_ai_cat_install = tx.clone();
    let dir_ai_cat = appimage_dir_state.clone();
    window.on_install_appimage_catalog(move |github| {
        let github = github.to_string();
        let tx = tx_ai_cat_install.clone();
        let dir = dir_ai_cat.lock().unwrap().clone();
        thread::spawn(move || {
            let title = format!("Installing {}", github);
            run_appimage_op(&tx, &title, Some(dir), None, |backend, log| {
                backend.install_from_github(&github, log).map(|_| ())
            });
        });
    });

    let dir_state_change = appimage_dir_state.clone();
    let win_ai_dir = window.as_weak();
    window.on_change_appimage_dir(move || {
        let dir_state = dir_state_change.clone();
        let win = win_ai_dir.clone();
        thread::spawn(move || {
            if let Some(path) = pick_directory() {
                *dir_state.lock().unwrap() = path.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(w) = win.upgrade() {
                        w.set_setting_appimage_dir(SharedString::from(path.as_str()));
                        w.invoke_save_settings();
                    }
                });
            }
        });
    });

    let dir_state_reset = appimage_dir_state.clone();
    let win_ai_dir_reset = window.as_weak();
    window.on_reset_appimage_dir(move || {
        *dir_state_reset.lock().unwrap() = String::new();
        if let Some(w) = win_ai_dir_reset.upgrade() {
            w.set_setting_appimage_dir(SharedString::from(""));
            w.invoke_save_settings();
        }
    });

    let apply_sources = {
        let src_state = appimage_sources_state.clone();
        let cat = appimage_catalog.clone();
        move |w: &MainWindow, feeds: Vec<AppImageFeed>| {
            *src_state.lock().unwrap() = feeds.clone();
            let model: Vec<AppImageSource> = feeds
                .iter()
                .map(|f| AppImageSource {
                    name: SharedString::from(f.name.as_str()),
                    url: SharedString::from(f.url.as_str()),
                })
                .collect();
            w.set_appimage_sources(ModelRc::new(VecModel::from(model)));
            cat.lock().unwrap().clear();
            w.set_catalog_appimages(ModelRc::new(VecModel::from(Vec::<AppImageCard>::new())));
            w.invoke_save_settings();
            w.invoke_load_appimage_catalog();
        }
    };

    let src_add = appimage_sources_state.clone();
    let win_src_add = window.as_weak();
    let apply_add = apply_sources.clone();
    window.on_add_appimage_source(move |name, url| {
        let url = url.trim().to_string();
        let mut name = name.trim().to_string();
        if url.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
            return;
        }
        if name.is_empty() {
            name = "Source".to_string();
        }
        let mut list = src_add.lock().unwrap().clone();
        if list.iter().any(|f| f.url == url) {
            return;
        }
        list.push(AppImageFeed { name, url });
        if let Some(w) = win_src_add.upgrade() {
            apply_add(&w, list);
        }
    });

    let src_rm = appimage_sources_state.clone();
    let win_src_rm = window.as_weak();
    let apply_rm = apply_sources.clone();
    window.on_remove_appimage_source(move |name| {
        let name = name.to_string();
        let default_url = xpm_appimage::catalog::FEED_URL;
        if src_rm.lock().unwrap().iter().any(|f| f.name == name && f.url == default_url) {
            return;
        }
        let list: Vec<AppImageFeed> =
            src_rm.lock().unwrap().iter().filter(|f| f.name != name).cloned().collect();
        if let Some(w) = win_src_rm.upgrade() {
            apply_rm(&w, list);
        }
    });

    let icon_inflight: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    let icon_jobs: mpsc::Sender<(String, String, std::path::PathBuf)> = {
        let (job_tx, job_rx) = mpsc::channel::<(String, String, std::path::PathBuf)>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..6 {
            let rx = job_rx.clone();
            let tx_done = tx.clone();
            let inflight = icon_inflight.clone();
            thread::spawn(move || loop {
                let job = { rx.lock().unwrap().recv() };
                let Ok((github, url, path)) = job else { break };
                let thumb = icons::thumb_path(&path);
                let mut pixels = icons::read_thumb(&thumb);
                if pixels.is_none() {
                    if !path.exists() {
                        if let Some(parent) = path.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::process::Command::new("curl")
                            .args(["-fsSL", "--max-time", "20", "-o"])
                            .arg(&path)
                            .arg(&url)
                            .status();
                    }
                    pixels = path.exists().then(|| icons::make_thumb(&path, &thumb)).flatten();
                }
                let message = match pixels {
                    Some(pixels) => Some(UiMessage::AppImageIconPixels { github: github.clone(), pixels }),
                    None if path.exists() => Some(UiMessage::AppImageIconReady {
                        github: github.clone(),
                        path: path.to_string_lossy().into_owned(),
                    }),
                    None => None,
                };
                inflight.lock().unwrap().remove(&github);
                if let Some(message) = message {
                    let _ = tx_done.send(message);
                }
            });
        }
        job_tx
    };

    let cat_icon = appimage_catalog.clone();
    window.on_load_appimage_icon(move |github| {
        let github = github.to_string();
        let url = {
            let cat = cat_icon.lock().unwrap();
            cat.iter().find(|e| e.github == github).and_then(|e| e.icon_url.clone())
        };
        let Some(url) = url else { return };
        if icon_inflight.lock().unwrap().insert(github.clone()) {
            let path = icon_cache_path(&github, &url);
            let _ = icon_jobs.send((github, url, path));
        }
    });

    let tx_export = tx.clone();
    let export_input = terminal_input_sender.clone();
    let export_pid = terminal_child_pid.clone();
    window.on_export_package_list(move || {
        info!("Data: Export Package List");
        let tx = tx_export.clone();
        let input = export_input.clone();
        let pid = export_pid.clone();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        thread::spawn(move || {
            let default_path = format!("{}/xpm-packages.txt", home);
            let chosen = std::process::Command::new("kdialog")
                .args(["--getsavefilename", &default_path, "*.txt", "--title", "Export Package List"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .or_else(|| {
                    std::process::Command::new("zenity")
                        .args([
                            "--file-selection", "--save", "--confirm-overwrite",
                            "--filename", &default_path,
                            "--title", "Export Package List",
                            "--file-filter", "Text files | *.txt",
                        ])
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                });

            let path = match chosen {
                Some(p) if !p.is_empty() => p,
                _ => {
                    return;
                }
            };

            let title = "Exporting Package List".to_string();
            let script = format!(
                "echo 'Collecting explicitly installed packages...'; \
                 pacman -Qqe > '{path}'; \
                 count=$(wc -l < '{path}'); \
                 echo \"Exported $count packages to {path}\"; \
                 echo ''; \
                 cat '{path}'"
            );
            run_in_terminal(&tx, &title, "bash", &["-c", &script], &input, &pid);
        });
    });

    let tx_import = tx.clone();
    let import_input = terminal_input_sender.clone();
    let import_pid = terminal_child_pid.clone();
    window.on_import_package_list(move || {
        info!("Data: Import Package List");
        let tx = tx_import.clone();
        let input = import_input.clone();
        let pid = import_pid.clone();
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        let path = format!("{}/xpm-packages.txt", home);
        thread::spawn(move || {
            let title = "Importing Package List".to_string();
            let script = format!(
                "if [ ! -f '{path}' ]; then echo 'File not found: {path}'; exit 1; fi; \
                 packages=$(cat '{path}' | awk '{{print $1}}' | tr '\\n' ' '); \
                 echo \"Installing: $packages\"; \
                 pacman -S --needed --noconfirm $packages"
            );
            run_in_terminal(&tx, &title, "pkexec", &["bash", "-c", &script], &input, &pid);
        });
    });

    let tx_mirrors = tx.clone();
    let mirror_input = terminal_input_sender.clone();
    let mirror_pid = terminal_child_pid.clone();
    window.on_update_mirrorlists(move || {
        info!("Troubleshoot: Update Mirrorlists");
        let tx = tx_mirrors.clone();
        let input = mirror_input.clone();
        let pid = mirror_pid.clone();
        thread::spawn(move || {
            let title = "Updating Mirrorlists".to_string();
            let script = build_mirrorlist_update_script();
            let args = ["bash", "-c", script.as_str()];
            run_in_terminal(&tx, &title, "pkexec", &args, &input, &pid);
        });
    });

    let tx_keyring = tx.clone();
    let keyring_input = terminal_input_sender.clone();
    let keyring_pid = terminal_child_pid.clone();
    window.on_fix_keyring(move || {
        info!("Troubleshoot: Fix GnuPG Keyring");
        let tx = tx_keyring.clone();
        let input = keyring_input.clone();
        let pid = keyring_pid.clone();
        thread::spawn(move || {
            let title = "Fixing GnuPG Keyring".to_string();
            run_in_terminal(&tx, &title, "pkexec", &["bash", "-c",
                            "rm -rf /etc/pacman.d/gnupg/* && pacman-key --init && pacman-key --populate && echo 'keyserver hkp://keyserver.ubuntu.com:80' | tee -a /etc/pacman.d/gnupg/gpg.conf && pacman -Syy --noconfirm archlinux-keyring"
            ], &input, &pid);
        });
    });

    let tx_initrd = tx.clone();
    let initrd_input = terminal_input_sender.clone();
    let initrd_pid = terminal_child_pid.clone();
    window.on_rebuild_initramfs(move || {
        info!("Troubleshoot: Rebuild InitRamFS");
        let tx = tx_initrd.clone();
        let input = initrd_input.clone();
        let pid = initrd_pid.clone();
        thread::spawn(move || {
            run_in_terminal(&tx, "Rebuild InitRamFS", "pkexec", &["mkinitcpio", "-P"], &input, &pid);
        });
    });

    let tx_grub = tx.clone();
    let grub_input = terminal_input_sender.clone();
    let grub_pid = terminal_child_pid.clone();
    window.on_rebuild_grub(move || {
        info!("Troubleshoot: Rebuild Grub");
        let tx = tx_grub.clone();
        let input = grub_input.clone();
        let pid = grub_pid.clone();
        thread::spawn(move || {
            run_in_terminal(&tx, "Rebuild GRUB Config", "pkexec", &["bash", "-c",
                "update-grub || grub-mkconfig -o /boot/grub/grub.cfg"
            ], &input, &pid);
        });
    });

    window.on_terminal_reboot(|| {
        info!("Reboot requested after upgrade");
        thread::spawn(|| {
            std::process::Command::new("systemctl")
                .arg("reboot")
                .spawn()
                .ok();
        });
    });

    let tx_remotes = tx.clone();
    let window_weak_remote = window.as_weak();
    let store_remote = flatpak_app_store.clone();
    let ids_remote = flatpak_installed_ids.clone();
    let loaded_remote = flatpak_loaded_remote.clone();
    window.on_browse_remote(move |remote| {
        let tx = tx_remotes.clone();
        let remote_str = remote.to_string();
        info!("Browse remote: {}", remote_str);

        // Fast path: serve from the in-memory store only when it holds this
        // remote's apps; otherwise fall through and reload.
        {
            let store = store_remote.lock().unwrap();
            let target = if remote_str.is_empty() { "flathub".to_string() } else { remote_str.clone() };
            let loaded = loaded_remote.lock().unwrap().clone();
            if !store.is_empty() && loaded == target {
                let ids = ids_remote.lock().unwrap();
                let all = apps_to_package_data(&store, &ids, &target, "All", "");
                let total = all.len();
                let page: Vec<PackageData> = all.into_iter().take(FLATPAK_PAGE_SIZE).collect();
                drop(ids);
                drop(store);
                let _ = tx.send(UiMessage::RemoteAppsFiltered { serial: u64::MAX, apps: page, total_matches: total });
                return;
            }
        }

        if let Some(w) = window_weak_remote.upgrade() {
            w.set_remote_apps_loading(true);
        }
        let tx2 = tx.clone();
        let remote2 = remote_str.clone();
        let store = store_remote.clone();
        let ids = ids_remote.clone();
        let loaded = loaded_remote.clone();
        thread::spawn(move || {
            let target = if remote2.is_empty() {
                let remotes = fetch_flatpak_remotes();
                let first = remotes.iter().find(|r| r.as_str() == "flathub").cloned()
                    .or_else(|| remotes.first().cloned())
                    .unwrap_or_else(|| "flathub".to_string());
                let _ = tx2.send(UiMessage::FlatpakRemotesLoaded(remotes));
                first
            } else {
                remote2
            };
            let (mut all_apps, mut installed) = load_remote_apps(&target);
            // First time browsing a remote (e.g. just-added one) its appstream may
            // not be cached yet, so the list is empty. Pull it once, then reload.
            if all_apps.is_empty() {
                let _ = std::process::Command::new("flatpak")
                    .args(["--user", "--noninteractive", "update", "--appstream", &target])
                    .status();
                let _ = std::fs::remove_file(remote_cache_path(&target));
                let (a, i) = load_remote_apps(&target);
                all_apps = a;
                installed = i;
            }
            *ids.lock().unwrap() = installed.clone();
            let all = apps_to_package_data(&all_apps, &installed, &target, "All", "");
            *store.lock().unwrap() = all_apps;
            *loaded.lock().unwrap() = target.clone();
            let total = all.len();
            let page: Vec<PackageData> = all.into_iter().take(FLATPAK_PAGE_SIZE).collect();
            let _ = tx.send(UiMessage::RemoteAppsFiltered { serial: u64::MAX, apps: page, total_matches: total });
        });
    });

    let tx_filter = tx.clone();
    let store_filter = flatpak_app_store.clone();
    let ids_filter = flatpak_installed_ids.clone();
    let window_weak_filter = window.as_weak();
    let serial_filter = flatpak_filter_serial.clone();
    window.on_filter_flatpak(move |category, search| {
        let cat = category.to_string();
        let q = search.to_string();
        let (remote, page) = if let Some(w) = window_weak_filter.upgrade() {
            (w.get_selected_remote().to_string(), w.get_flatpak_page().max(0) as usize)
        } else {
            ("flathub".to_string(), 0)
        };
        let my_serial = serial_filter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let store = store_filter.clone();
        let ids = ids_filter.clone();
        let tx = tx_filter.clone();
        let serial_check = serial_filter.clone();
        thread::spawn(move || {
            let store = store.lock().unwrap();
            let ids = ids.lock().unwrap();
            if serial_check.load(std::sync::atomic::Ordering::Relaxed) != my_serial {
                return;
            }
            let all = apps_to_package_data(&store, &ids, &remote, &cat, &q);
            drop(store);
            drop(ids);
            if serial_check.load(std::sync::atomic::Ordering::Relaxed) != my_serial {
                return;
            }
            let total = all.len();
            let page = page.min(total.saturating_sub(1) / FLATPAK_PAGE_SIZE);
            let apps: Vec<PackageData> = all
                .into_iter()
                .skip(page * FLATPAK_PAGE_SIZE)
                .take(FLATPAK_PAGE_SIZE)
                .collect();
            let _ = tx.send(UiMessage::RemoteAppsFiltered { serial: my_serial, apps, total_matches: total });
        });
    });

    let win_toggle_fk = window.as_weak();
    window.on_toggle_flatpak_selected(move |app_id, checked| {
        if let Some(w) = win_toggle_fk.upgrade() {
            let model = w.get_remote_apps();
            let updated: Vec<PackageData> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .map(|mut p| {
                    if p.name.as_str() == app_id.as_str() {
                        p.selected = checked;
                    }
                    p
                })
                .collect();
            let sel_count = updated.iter().filter(|p| p.selected).count() as i32;
            let sel_installed = updated.iter().filter(|p| p.selected && p.installed).count() as i32;
            let sel_uninstalled = updated.iter().filter(|p| p.selected && !p.installed).count() as i32;
            w.set_remote_apps(ModelRc::new(VecModel::from(updated)));
            w.set_selected_count(sel_count);
            w.set_selected_installed_count(sel_installed);
            w.set_selected_uninstalled_count(sel_uninstalled);
        }
    });

    let win_batch_fi = window.as_weak();
    let tx_bfi = tx.clone();
    let bfi_input = terminal_input_sender.clone();
    let bfi_pid = terminal_child_pid.clone();
    window.on_batch_flatpak_install(move || {
        if let Some(w) = win_batch_fi.upgrade() {
            let model = w.get_remote_apps();
            let ids: Vec<String> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .filter(|p| p.selected && !p.installed)
                .map(|p| p.name.to_string())
                .collect();
            if ids.is_empty() {
                return;
            }
            let tx = tx_bfi.clone();
            let input = bfi_input.clone();
            let pid = bfi_pid.clone();
            let title = format!("Installing {} Flatpak(s)", ids.len());
            thread::spawn(move || {
                let mut args = vec!["install", "flathub"];
                let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
                args.extend(id_refs.iter().copied());
                run_in_terminal(&tx, &title, "flatpak", &args, &input, &pid);
            });
        }
    });

    let win_batch_fr = window.as_weak();
    let tx_bfr = tx.clone();
    let bfr_input = terminal_input_sender.clone();
    let bfr_pid = terminal_child_pid.clone();
    window.on_batch_flatpak_remove(move || {
        if let Some(w) = win_batch_fr.upgrade() {
            let model = w.get_remote_apps();
            let ids: Vec<String> = (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .filter(|p| p.selected && p.installed)
                .map(|p| p.name.to_string())
                .collect();
            if ids.is_empty() {
                return;
            }
            let tx = tx_bfr.clone();
            let input = bfr_input.clone();
            let pid = bfr_pid.clone();
            let title = format!("Removing {} Flatpak(s)", ids.len());
            thread::spawn(move || {
                let mut args = vec!["uninstall"];
                let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
                args.extend(id_refs.iter().copied());
                run_in_terminal(&tx, &title, "flatpak", &args, &input, &pid);
            });
        }
    });

    let tx_detail = tx.clone();
    let store_detail = flatpak_app_store.clone();
    let ids_detail = flatpak_installed_ids.clone();
    let remote_detail = flatpak_loaded_remote.clone();
    window.on_load_flatpak_detail(move |app_id| {
        let id = app_id.to_string();
        let store = store_detail.lock().unwrap();
        let installed = ids_detail.lock().unwrap();
        let Some(app) = store.iter().find(|a| a.app_id == id) else { return };
        let _ = tx_detail.send(UiMessage::FlatpakDetailReady(Box::new(FlatpakDetail {
            app_id: id.clone(),
            name: if app.name.is_empty() { app.app_id.clone() } else { app.name.clone() },
            summary: app.summary.clone(),
            description: app.description.clone(),
            developer: app.developer.clone(),
            version: app.version.clone(),
            version_date: app.version_date.clone(),
            license: app.license.clone(),
            url_homepage: app.url_homepage.clone(),
            url_bugtracker: app.url_bugtracker.clone(),
            url_translate: app.url_translate.clone(),
            url_vcs: app.url_vcs.clone(),
            url_donation: app.url_donation.clone(),
            url_help: app.url_help.clone(),
            categories: app.categories.clone(),
            releases: app.releases.iter().map(|r| (r.version.clone(), r.date.clone(), r.notes.clone())).collect(),
            captions: app.screenshots.iter().map(|s| s.caption.clone()).collect(),
        })));
        let mut seen_addon = std::collections::HashSet::new();
        let addons: Vec<PackageData> = store.iter()
            .filter(|a| a.extends == id)
            .filter(|a| seen_addon.insert(a.app_id.clone()))
            .map(|a| PackageData {
                name: SharedString::from(a.app_id.as_str()),
                display_name: SharedString::from(if a.name.is_empty() { &a.app_id } else { &a.name }),
                version: SharedString::from(""),
                description: SharedString::from(a.summary.as_str()),
                repository: SharedString::from(""),
                backend: 1,
                installed: installed.contains(&a.app_id),
                has_update: false,
                installed_size: SharedString::from(""),
                licenses: SharedString::from(""),
                url: SharedString::from(""),
                dependencies: SharedString::from(""),
                required_by: SharedString::from(""),
                selected: false,
                explicit: false,
            })
            .collect();
        let _ = tx_detail.send(UiMessage::FlatpakAddonsReady(addons));

        let shots: Vec<String> = app.screenshots.iter().map(|s| s.url.clone()).collect();
        if !shots.is_empty() {
            let (tx_ss, ss_id) = (tx_detail.clone(), id.clone());
            thread::spawn(move || {
                for (index, url) in shots.iter().enumerate() {
                    let dest = flathub::screenshot_path(&ss_id, index, url);
                    if flathub::download_screenshot(url, &dest) {
                        let _ = tx_ss.send(UiMessage::FlatpakScreenshotReady {
                            app_id: ss_id.clone(),
                            index,
                            path: dest.to_string_lossy().into_owned(),
                        });
                    }
                }
            });
        }

        if *remote_detail.lock().unwrap() == "flathub" {
            let tx_extras = tx_detail.clone();
            thread::spawn(move || {
                if let Some(extras) = flathub::fetch_extras(&id) {
                    let _ = tx_extras.send(UiMessage::FlatpakExtrasReady { app_id: id, extras });
                }
            });
        }
    });

    let store_icon = flatpak_app_store.clone();
    let tx_icon = tx.clone();
    let loaded_icon = flatpak_loaded_remote.clone();
    window.on_load_flatpak_icon(move |app_id| {
        let id = app_id.to_string();
        let icon_name = {
            let store = store_icon.lock().unwrap();
            store.iter().find(|a| a.app_id == id).map(|a| a.icon_name.clone()).unwrap_or_default()
        };
        if !icon_name.is_empty() {
            let remote = loaded_icon.lock().unwrap().clone();
            // Try the currently-loaded remote's appstream dir (user or system), then flathub.
            for r in [remote.as_str(), "flathub"] {
                if r.is_empty() { continue; }
                let path = format!("{}/icons/128x128/{}", appstream_base(r), icon_name);
                if std::path::Path::new(&path).exists() {
                    let _ = tx_icon.send(UiMessage::FlatpakIconReady(path));
                    break;
                }
            }
        }
    });

    let tx_repos = tx.clone();
    window.on_load_pacman_repos(move || {
        let tx = tx_repos.clone();
        thread::spawn(move || {
            let repos = load_pacman_repos();
            let _ = tx.send(UiMessage::PacmanReposLoaded(repos));
            let pkgs = load_repo_packages("");
            let _ = tx.send(UiMessage::RepoPackagesLoaded(pkgs));
        });
    });

    let tx_repo_pkgs = tx.clone();
    let window_weak_repo = window.as_weak();

    let repo_browse_clear = repo_packages_full.clone();
    window.on_browse_repo(move |repo| {
        let tx = tx_repo_pkgs.clone();
        let repo_str = repo.to_string();
        info!("Browse repo: {}", repo_str);
        *repo_browse_clear.borrow_mut() = Vec::new();
        if let Some(w) = window_weak_repo.upgrade() {
            w.set_repo_loading(true);
            w.set_show_repo_detail(false);
            w.set_repo_page(0);
        }
        thread::spawn(move || {
            let pkgs = load_repo_packages(&repo_str);
            let _ = tx.send(UiMessage::RepoPackagesLoaded(pkgs));
        });
    });

    let repo_full_filter = repo_packages_full.clone();
    let win_filter_repo = window.as_weak();
    window.on_filter_repo(move |search| {
        let full = repo_full_filter.borrow();
        let filtered = filter_repo_list(&full, search.as_str());
        drop(full);
        if let Some(w) = win_filter_repo.upgrade() {
            render_repo_page(&w, &filtered, 0);
        }
    });

    let repo_full_goto = repo_packages_full.clone();
    let win_goto_repo = window.as_weak();
    window.on_goto_repo_page(move |page| {
        if let Some(w) = win_goto_repo.upgrade() {
            let full = repo_full_goto.borrow();
            let filtered = filter_repo_list(&full, w.get_repo_search().as_str());
            drop(full);
            render_repo_page(&w, &filtered, page.max(0) as usize);
        }
    });

    let tx_pkg_info = tx.clone();
    let window_weak_pi = window.as_weak();
    window.on_load_pkg_info(move |name| {
        let tx = tx_pkg_info.clone();
        let n = name.to_string();
        if let Some(w) = window_weak_pi.upgrade() {
            w.set_pkg_info_loading(true);
            w.set_pkg_info_files(SharedString::from(""));
        }
        thread::spawn(move || {
            let ql = std::process::Command::new("pacman")
                .args(["-Ql", &n])
                .output();
            let text = match ql {
                Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .filter_map(|l| l.split_once(' ').map(|x| x.1))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
                _ => {
                    let fl = std::process::Command::new("pacman")
                        .args(["-Fl", &n])
                        .output();
                    match fl {
                        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                            String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .filter_map(|l| {
                                    let mut parts = l.splitn(2, ' ');
                                    parts.next();
                                    parts.next().map(|p| format!("/{}", p))
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                        _ => "File database not synced.\nRun: sudo pacman -Fy".to_string(),
                    }
                }
            };
            let _ = tx.send(UiMessage::PkgInfoLoaded(text));
        });
    });

    let tx_fk_info = tx.clone();
    let window_weak_fki = window.as_weak();
    window.on_load_flatpak_info(move |app_id| {
        let tx = tx_fk_info.clone();
        let id = app_id.to_string();
        if let Some(w) = window_weak_fki.upgrade() {
            w.set_pkg_info_loading(true);
            w.set_pkg_info_files(SharedString::from(""));
        }
        thread::spawn(move || {
            let text = match std::process::Command::new("flatpak")
                .args(["info", "--show-location", &id])
                .output()
            {
                Ok(loc) => {
                    let base = String::from_utf8_lossy(&loc.stdout).trim().to_string();
                    if base.is_empty() {
                        format!("Could not locate install path for {}", id)
                    } else {
                        let files_path = format!("{}/files", base);
                        match std::process::Command::new("find")
                            .args([&files_path, "!", "-type", "d"])
                            .output()
                        {
                            Ok(f) => {
                                let raw = String::from_utf8_lossy(&f.stdout).trim().to_string();
                                if raw.is_empty() {
                                    format!("No files found under {}", files_path)
                                } else {
                                    raw
                                }
                            }
                            Err(_) => format!("Could not list files under {}", files_path),
                        }
                    }
                }
                Err(_) => format!("Could not locate install path for {}", id),
            };
            let _ = tx.send(UiMessage::PkgInfoLoaded(text));
        });
    });

    let tx_repo_detail = tx.clone();
    let window_weak_rd = window.as_weak();
    window.on_select_repo_pkg(move |name, _backend| {
        let tx = tx_repo_detail.clone();
        let pkg = name.to_string();
        if let Some(w) = window_weak_rd.upgrade() {
            w.set_repo_detail_loading(true);
            w.set_repo_detail_description(SharedString::from(""));
        }
        thread::spawn(move || {
            let out = std::process::Command::new("pacman")
                .args(["-Si", &pkg])
                .output();
            let desc = match out {
                Ok(o) => {
                    let text = String::from_utf8_lossy(&o.stdout).to_string();
                    text.lines()
                        .find(|l| l.starts_with("Description"))
                        .and_then(|l| l.split_once(':').map(|x| x.1))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default()
                }
                Err(_) => String::new(),
            };
            let _ = tx.send(UiMessage::RepoPkgDetail(desc));
        });
    });

    let window_weak_conf = window.as_weak();
    window.on_conflict_cancel(move || {
        if let Some(w) = window_weak_conf.upgrade() {
            w.set_show_conflict_dialog(false);
        }
    });

    let tx_force = tx.clone();
    let force_input = terminal_input_sender.clone();
    let force_pid = terminal_child_pid.clone();
    let force_ctx = conflict_context.clone();
    let window_weak_force = window.as_weak();
    window.on_conflict_force_overwrite(move || {
        let ctx = force_ctx.lock().unwrap().clone();
        if let Some((action, names, backend)) = ctx {
            if let Some(w) = window_weak_force.upgrade() {
                w.set_show_conflict_dialog(false);
            }
            let tx = tx_force.clone();
            let input = force_input.clone();
            let pid = force_pid.clone();
            let ctx2 = force_ctx.clone();
            let force_action = match action.as_str() {
                "update-all" => "force-update-all".to_string(),
                _ => "force-install".to_string(),
            };
            let title = format!("Force Installing {} package(s)", names.len());
            thread::spawn(move || {
                run_managed_operation(&tx, &title, &force_action, &names, backend, &input, &pid, &ctx2);
            });
        }
    });

    let window_weak_ss = window.as_weak();
    let window_weak_grp = window.as_weak();
    let full_grp_loader = full_installed_grouped.clone();
    window.on_load_installed_grouped(move || {
        if let Some(w) = window_weak_grp.upgrade() {
            let pkgs: Vec<PackageData> = w.get_installed_packages().iter().collect();
            let grouped = group_installed_by_repo(pkgs);
            *full_grp_loader.borrow_mut() = grouped.clone();
            w.set_installed_grouped(ModelRc::new(VecModel::from(grouped)));
        }
    });

    let window_weak_fig = window.as_weak();
    let full_grp_filter = full_installed_grouped.clone();
    window.on_filter_installed_grouped(move |query| {
        if let Some(w) = window_weak_fig.upgrade() {
            let q = query.to_string().to_lowercase();
            let data = full_grp_filter.borrow();
            let filtered: Vec<PackageData> = if q.is_empty() {
                data.clone()
            } else {
                let mut result = Vec::new();
                let mut current_header: Option<PackageData> = None;
                let mut header_has_match = false;
                for item in data.iter() {
                    if item.backend == -1 {
                        if let Some(h) = current_header.take()
                            && header_has_match {
                                result.push(h);
                            }
                        current_header = Some(item.clone());
                        header_has_match = false;
                    } else {
                        let matches = item.name.to_lowercase().contains(&q)
                            || item.display_name.to_lowercase().contains(&q);
                        if matches {
                            if let Some(ref h) = current_header
                                && !header_has_match {
                                    result.push(h.clone());
                                    header_has_match = true;
                                }
                            result.push(item.clone());
                        }
                    }
                }
                result
            };
            w.set_installed_grouped(ModelRc::new(VecModel::from(filtered)));
        }
    });

    let window_weak_ef = window.as_weak();
    let full_grp_ef = full_installed_grouped.clone();
    window.on_apply_explicit_filter(move |mode| {
        if let Some(w) = window_weak_ef.upgrade() {
            let data = full_grp_ef.borrow();
            let filtered: Vec<PackageData> = if mode == 0 {
                data.clone()
            } else {
                let want_explicit = mode == 1;
                let mut result = Vec::new();
                let mut current_header: Option<PackageData> = None;
                let mut header_has_match = false;
                for item in data.iter() {
                    if item.backend == -1 {
                        if let Some(h) = current_header.take()
                            && header_has_match {
                                result.push(h);
                            }
                        current_header = Some(item.clone());
                        header_has_match = false;
                    } else {
                        let matches = item.explicit == want_explicit;
                        if matches {
                            if let Some(ref h) = current_header
                                && !header_has_match {
                                    result.push(h.clone());
                                    header_has_match = true;
                                }
                            result.push(item.clone());
                        }
                    }
                }
                if let Some(h) = current_header
                    && header_has_match {
                        result.push(h);
                    }
                result
            };
            w.set_installed_grouped(ModelRc::new(VecModel::from(filtered)));
        }
    });

    let window_weak_lall = window.as_weak();
    let full_installed_lall = full_installed.clone();
    let full_grp_lall = full_installed_grouped.clone();
    window.on_load_all_installed_grouped(move || {
        if let Some(w) = window_weak_lall.upgrade() {
            let pkgs = full_installed_lall.borrow().clone();
            let grouped = group_installed_by_repo(pkgs);
            *full_grp_lall.borrow_mut() = grouped.clone();
            w.set_installed_grouped(ModelRc::new(VecModel::from(grouped)));
        }
    });

    let tx_dg = tx.clone();
    let dg_input = terminal_input_sender.clone();
    let dg_pid = terminal_child_pid.clone();
    window.on_dismiss_warning_popup(|| {});

    let tx_idg = tx.clone();
    let idg_input = terminal_input_sender.clone();
    let idg_pid = terminal_child_pid.clone();
    window.on_install_downgrade(move || {
        let tx = tx_idg.clone();
        let input = idg_input.clone();
        let pid = idg_pid.clone();
        thread::spawn(move || {
            run_in_terminal(
                &tx,
                "Install downgrade",
                "pkexec",
                &["pacman", "-S", "--noconfirm", "downgrade"],
                &input,
                &pid,
            );
        });
    });

    window.on_downgrade_package(move |pkg_name| {
        let name = pkg_name.to_string();
        info!("Downgrade: {}", name);
        let tx = tx_dg.clone();

        let Some(dg_bin) = xpm_core::resolve_tool("downgrade") else {
            let _ = tx.send(UiMessage::ShowWarning {
                message: "The downgrade package is not installed on this system.\n\nThis feature requires it to function.".to_string(),
                chaotic_aur: is_chaotic_aur_enabled(),
            });
            return;
        };

        let input = dg_input.clone();
        let pid = dg_pid.clone();
        thread::spawn(move || {
            let fake_dir = format!("/tmp/xpm-fzf-{}", std::process::id());
            let fzf_path = format!("{}/fzf", fake_dir);
            let _ = std::fs::create_dir_all(&fake_dir);
            let _ = std::fs::write(&fzf_path, FAKE_FZF_SCRIPT);
            if let Ok(meta) = std::fs::metadata(&fzf_path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(&fzf_path, perms);
            }
            let bash_cmd = format!(
                "export PATH={dir}:\"$PATH\"; {dg} {name}",
                dir = fake_dir,
                dg = dg_bin.display(),
                name = name,
            );
            run_in_terminal(
                &tx,
                &format!("Downgrade {}", name),
                "pkexec",
                &["bash", "-c", &bash_cmd],
                &input,
                &pid,
            );
            let _ = std::fs::remove_dir_all(&fake_dir);
        });
    });

    let tx_ifk = tx.clone();
    window.on_load_installed_flatpaks(move || {
        let tx = tx_ifk.clone();
        thread::spawn(move || {
            let pkgs = load_installed_flatpaks();
            let _ = tx.send(UiMessage::InstalledFlatpaksLoaded(pkgs));
        });
    });

    // ---- Flatpak permission editor callbacks ----
    let perm_ctx = Arc::new(Mutex::new(PermCtx::default()));
    let perm_apps_all = Arc::new(Mutex::new(Vec::<PermApp>::new()));

    {
        let ctx = perm_ctx.clone();
        let apps_all = perm_apps_all.clone();
        let weak = window.as_weak();
        window.on_open_flatpak_perms(move |id, name| {
            let Some(w) = weak.upgrade() else { return };
            let apps = perm_app_list();
            *apps_all.lock().unwrap() = apps.clone();
            w.set_perm_apps(ModelRc::new(VecModel::from(apps)));
            w.set_perm_app_filter(SharedString::new());
            w.set_perm_scope_system(false);
            w.set_perm_dirty(false);
            w.set_perm_selected_id(id.clone());
            w.set_perm_selected_name(name.clone());
            w.set_perm_loading(true);
            w.set_show_flatpak_perms_modal(true);
            ctx.lock().unwrap().scope_system = false;
            perm_load(weak.clone(), ctx.clone(), id.to_string());
        });
    }

    {
        let ctx = perm_ctx.clone();
        let apps_all = perm_apps_all.clone();
        let weak = window.as_weak();
        window.on_open_flatpak_perms_manager(move || {
            let Some(w) = weak.upgrade() else { return };
            let apps = perm_app_list();
            *apps_all.lock().unwrap() = apps.clone();
            let first = apps.first().cloned();
            w.set_perm_apps(ModelRc::new(VecModel::from(apps)));
            w.set_perm_app_filter(SharedString::new());
            w.set_perm_scope_system(false);
            w.set_perm_dirty(false);
            w.set_show_flatpak_perms_modal(true);
            ctx.lock().unwrap().scope_system = false;
            match first {
                Some(a) => {
                    w.set_perm_selected_id(a.id.clone());
                    w.set_perm_selected_name(a.name.clone());
                    w.set_perm_loading(true);
                    perm_load(weak.clone(), ctx.clone(), a.id.to_string());
                }
                None => {
                    w.set_perm_selected_id(SharedString::new());
                    w.set_perm_selected_name(SharedString::new());
                }
            }
        });
    }

    {
        let ctx = perm_ctx.clone();
        let apps_all = perm_apps_all.clone();
        let weak = window.as_weak();
        window.on_select_perm_app(move |id| {
            let Some(w) = weak.upgrade() else { return };
            let id_s = id.to_string();
            let name = apps_all.lock().unwrap().iter()
                .find(|a| a.id == id)
                .map(|a| a.name.clone())
                .unwrap_or_else(|| SharedString::from(id_s.as_str()));
            w.set_perm_selected_id(SharedString::from(id_s.as_str()));
            w.set_perm_selected_name(name);
            w.set_perm_loading(true);
            perm_load(weak.clone(), ctx.clone(), id_s);
        });
    }

    {
        let apps_all = perm_apps_all.clone();
        let weak = window.as_weak();
        window.on_filter_perm_apps(move |q| {
            let Some(w) = weak.upgrade() else { return };
            let ql = q.to_string().to_lowercase();
            let filtered: Vec<PermApp> = apps_all.lock().unwrap().iter()
                .filter(|a| ql.is_empty() || a.name.to_lowercase().contains(&ql) || a.id.to_lowercase().contains(&ql))
                .cloned()
                .collect();
            w.set_perm_apps(ModelRc::new(VecModel::from(filtered)));
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_set_perm_scope(move |system| {
            let Some(w) = weak.upgrade() else { return };
            let id = {
                let mut c = ctx.lock().unwrap();
                c.scope_system = system;
                c.pending.clear();
                c.id.clone()
            };
            w.set_perm_scope_system(system);
            w.set_perm_dirty(false);
            if id.is_empty() { return; }
            w.set_perm_loading(true);
            perm_load(weak.clone(), ctx.clone(), id);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_toggle_perm(move |category, token, on| {
            let Some(w) = weak.upgrade() else { return };
            let flag = {
                let mut c = ctx.lock().unwrap();
                let (cat, key) = match category.as_str() {
                    "shared" => (fperm::Category::Shared, "shared"),
                    "socket" => (fperm::Category::Socket, "sockets"),
                    "device" => (fperm::Category::Device, "devices"),
                    "feature" => (fperm::Category::Feature, "features"),
                    "filesystem" => (fperm::Category::Filesystem, "filesystems"),
                    _ => return,
                };
                kf_ctx_set(&mut c.working, key, token.as_str(), on);
                fperm::toggle_arg(cat, token.as_str(), on, None)
            };
            perm_after_edit(&w, &ctx, flag);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_add_perm_fs(move |path, mode| {
            let Some(w) = weak.upgrade() else { return };
            let p = path.to_string();
            let m = mode.to_string();
            if p.is_empty() { return; }
            let flag = {
                let mut c = ctx.lock().unwrap();
                kf_fs_add(&mut c.working, &p, &m);
                let mode_opt = if m.is_empty() || m == "rw" { None } else { Some(m.as_str()) };
                fperm::toggle_arg(fperm::Category::Filesystem, &p, true, mode_opt)
            };
            perm_after_edit(&w, &ctx, flag);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_remove_perm_fs(move |path| {
            let Some(w) = weak.upgrade() else { return };
            let p = path.to_string();
            let flag = {
                let mut c = ctx.lock().unwrap();
                kf_fs_remove(&mut c.working, &p);
                fperm::toggle_arg(fperm::Category::Filesystem, &p, false, None)
            };
            perm_after_edit(&w, &ctx, flag);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_add_perm_bus(move |system_bus, name| {
            let Some(w) = weak.upgrade() else { return };
            let n = name.to_string();
            if n.is_empty() { return; }
            let flag = {
                let mut c = ctx.lock().unwrap();
                let section = if system_bus { fperm::SYSTEM_BUS } else { fperm::SESSION_BUS };
                kf_bus_set(&mut c.working, section, &n, true);
                fperm::bus_arg(system_bus, &n, true)
            };
            perm_after_edit(&w, &ctx, flag);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_remove_perm_bus(move |system_bus, name| {
            let Some(w) = weak.upgrade() else { return };
            let n = name.to_string();
            let flag = {
                let mut c = ctx.lock().unwrap();
                let section = if system_bus { fperm::SYSTEM_BUS } else { fperm::SESSION_BUS };
                kf_bus_set(&mut c.working, section, &n, false);
                fperm::bus_arg(system_bus, &n, false)
            };
            perm_after_edit(&w, &ctx, flag);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_add_perm_env(move |key, value| {
            let Some(w) = weak.upgrade() else { return };
            let k = key.to_string();
            let v = value.to_string();
            if k.is_empty() { return; }
            let flag = {
                let mut c = ctx.lock().unwrap();
                kf_env_set(&mut c.working, &k, Some(&v));
                fperm::env_arg(&k, Some(&v))
            };
            perm_after_edit(&w, &ctx, flag);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_remove_perm_env(move |key| {
            let Some(w) = weak.upgrade() else { return };
            let k = key.to_string();
            let flag = {
                let mut c = ctx.lock().unwrap();
                kf_env_set(&mut c.working, &k, None);
                fperm::env_arg(&k, None)
            };
            perm_after_edit(&w, &ctx, flag);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_add_perm_persist(move |path| {
            let Some(w) = weak.upgrade() else { return };
            let p = path.to_string();
            if p.is_empty() { return; }
            let flag = {
                let mut c = ctx.lock().unwrap();
                kf_persist_add(&mut c.working, &p);
                fperm::persist_arg(&p)
            };
            perm_after_edit(&w, &ctx, flag);
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_reset_perm(move || {
            let Some(w) = weak.upgrade() else { return };
            let (id, system) = {
                let c = ctx.lock().unwrap();
                (c.id.clone(), c.scope_system)
            };
            if id.is_empty() { return; }
            w.set_perm_loading(true);
            let argv = fperm::reset_argv(system, &id);
            let ctx2 = ctx.clone();
            let weak2 = weak.clone();
            let id2 = id.clone();
            thread::spawn(move || {
                if system {
                    let _ = std::process::Command::new("pkexec").arg("flatpak").args(&argv).status();
                } else {
                    let _ = std::process::Command::new("flatpak").args(&argv).status();
                }
                perm_load(weak2, ctx2, id2);
            });
        });
    }

    {
        let ctx = perm_ctx.clone();
        let weak = window.as_weak();
        window.on_apply_perm_system(move || {
            let Some(w) = weak.upgrade() else { return };
            let (id, flags) = {
                let c = ctx.lock().unwrap();
                (c.id.clone(), c.pending.clone())
            };
            if id.is_empty() || flags.is_empty() { return; }
            w.set_perm_loading(true);
            let argv = fperm::override_argv(true, &id, &flags);
            let ctx2 = ctx.clone();
            let weak2 = weak.clone();
            let id2 = id.clone();
            thread::spawn(move || {
                let _ = std::process::Command::new("pkexec").arg("flatpak").args(&argv).status();
                perm_load(weak2, ctx2, id2);
            });
        });
    }

    // ---- Transaction history callbacks ----
    let history_txns = Arc::new(Mutex::new(Vec::<alpmhist::Transaction>::new()));

    {
        let store = history_txns.clone();
        let weak = window.as_weak();
        window.on_open_transaction_history(move || {
            let Some(w) = weak.upgrade() else { return };
            // Gate behind the warning unless the user opted out previously.
            if load_config().history_warn_dismissed {
                load_history(weak.clone(), store.clone());
            } else {
                w.set_history_snapshot_note(SharedString::from(snapshot_note()));
                w.set_history_warn_dontshow(false);
                w.set_history_warn_open(true);
            }
        });
    }

    {
        let store = history_txns.clone();
        let weak = window.as_weak();
        window.on_proceed_history_warning(move |dontshow| {
            if dontshow {
                let mut cfg = load_config();
                cfg.history_warn_dismissed = true;
                save_config(&cfg);
            }
            load_history(weak.clone(), store.clone());
        });
    }

    {
        let store = history_txns.clone();
        let weak = window.as_weak();
        window.on_select_transaction(move |idx| {
            let Some(w) = weak.upgrade() else { return };
            let i = idx as usize;
            let txns = store.lock().unwrap();
            let Some(t) = txns.get(i) else { return };
            let actions: Vec<HistAction> = t.actions.iter().map(|a| {
                let change = match a.kind {
                    alpmhist::ActionKind::Removed => a.old.clone(),
                    alpmhist::ActionKind::Installed | alpmhist::ActionKind::Reinstalled => a.new.clone(),
                    _ => format!("{} -> {}", a.old, a.new),
                };
                HistAction { kind: a.kind.label().into(), pkg: a.pkg.as_str().into(), change: change.into() }
            }).collect();
            w.set_history_selected(idx);
            w.set_history_detail_title(SharedString::from(format!("{}  -  {}", pretty_when(&t.when), txn_summary(t))));
            w.set_history_can_rollback(!t.upgraded_targets().is_empty());
            w.set_history_actions(ModelRc::new(VecModel::from(actions)));
        });
    }

    {
        let store = history_txns.clone();
        let weak = window.as_weak();
        let tx = tx.clone();
        let input = terminal_input_sender.clone();
        let pid = terminal_child_pid.clone();
        window.on_confirm_rollback(move || {
            let Some(w) = weak.upgrade() else { return };
            let idx = w.get_history_selected();
            if idx < 0 { return; }
            let targets = {
                let txns = store.lock().unwrap();
                match txns.get(idx as usize) {
                    Some(t) => t.upgraded_targets(),
                    None => return,
                }
            };
            if targets.is_empty() { return; }
            w.set_show_history_modal(false);
            let tx = tx.clone();
            let input = input.clone();
            let pid = pid.clone();
            thread::spawn(move || {
                let mut files = Vec::new();
                let mut missing = Vec::new();
                for (name, ver) in &targets {
                    match resolve_old_pkg(name, ver) {
                        Some(p) => files.push(p),
                        None => missing.push(format!("{name} {ver}")),
                    }
                }
                if files.is_empty() {
                    let _ = tx.send(UiMessage::ShowProgressPopup("Rollback".to_string()));
                    let _ = tx.send(UiMessage::ProgressOutput(
                        "No previous versions found in cache or the Arch Linux Archive. Nothing to roll back.\n".to_string(),
                    ));
                    let _ = tx.send(UiMessage::OperationDone(false));
                    return;
                }
                // Build an interactive pacman -U over the resolved files. No --noconfirm:
                // pacman shows its plan and the user confirms in the popup. No force flags.
                let mut script = String::new();
                if !missing.is_empty() {
                    script.push_str(&format!(
                        "echo 'Skipped (no cached/Archive version): {}'; echo; ",
                        missing.join(", ")
                    ));
                }
                script.push_str("pacman -U");
                for f in &files {
                    script.push(' ');
                    script.push_str(f);
                }
                // Remind the user (Option 2): the rollback is not held, so a future
                // update will bring these versions back.
                script.push_str(
                    " && echo '' \
                     && echo '== Rollback complete ==' \
                     && echo 'These packages are NOT held. Your next system update will upgrade them again and re-apply this change.' \
                     && echo 'Do NOT run a full update until you have identified/fixed the cause or taken a Timeshift / Btrfs snapshot.' \
                     && echo 'If the newer version still breaks things, just roll back again.'",
                );
                run_in_terminal(&tx, "Rolling back update", "pkexec", &["bash", "-c", &script], &input, &pid);
            });
        });
    }


    let win_toggle = window.as_weak();
    window.on_toggle_addon_selected(move |idx| {
        let Some(w) = win_toggle.upgrade() else { return };
        let model = w.get_addon_selected();
        let i = idx as usize;
        if i >= model.row_count() { return; }
        let current = model.row_data(i).unwrap_or(false);
        let new_val = !current;
        model.set_row_data(i, new_val);
        let delta: i32 = if new_val { 1 } else { -1 };
        w.set_addon_selected_count((w.get_addon_selected_count() + delta).max(0));
    });

    let win_selall = window.as_weak();
    window.on_addon_select_all(move |select| {
        let Some(w) = win_selall.upgrade() else { return };
        let model = w.get_addon_selected();
        let count = model.row_count() as i32;
        for i in 0..model.row_count() {
            model.set_row_data(i, select);
        }
        w.set_addon_selected_count(if select { count } else { 0 });
    });

    let win_inst_addons = window.as_weak();
    let tx_inst_addons = tx.clone();
    let inst_addons_input = terminal_input_sender.clone();
    let inst_addons_pid = terminal_child_pid.clone();
    let inst_addons_ctx = conflict_context.clone();
    window.on_install_selected_addons(move || {
        let Some(w) = win_inst_addons.upgrade() else { return };
        let addons = w.get_flatpak_addons();
        let selected = w.get_addon_selected();
        let ids: Vec<String> = (0..addons.row_count())
            .filter(|&i| selected.row_data(i).unwrap_or(false))
            .filter_map(|i| addons.row_data(i))
            .map(|a| a.name.to_string())
            .collect();
        if ids.is_empty() { return; }
        let title = format!("Installing {} add-on(s)", ids.len());
        let tx = tx_inst_addons.clone();
        let input = inst_addons_input.clone();
        let pid = inst_addons_pid.clone();
        let ctx = inst_addons_ctx.clone();
        thread::spawn(move || {
            run_managed_operation(&tx, &title, "bulk-install", &ids, 1, &input, &pid, &ctx);
        });
    });

    let tx_rem_addon = tx.clone();
    let rem_addon_input = terminal_input_sender.clone();
    let rem_addon_pid = terminal_child_pid.clone();
    let rem_addon_ctx = conflict_context.clone();
    window.on_remove_addon(move |id| {
        let id_str = id.to_string();
        let tx = tx_rem_addon.clone();
        let input = rem_addon_input.clone();
        let pid = rem_addon_pid.clone();
        let ctx = rem_addon_ctx.clone();
        thread::spawn(move || {
            run_managed_operation(
                &tx,
                &format!("Removing {}", id_str),
                "remove",
                &[id_str],
                1,
                &input,
                &pid,
                &ctx,
            );
        });
    });

    let tx_deptree = tx.clone();
    window.on_load_dep_tree(move |pkg_name| {
        let name = pkg_name.to_string();
        let tx = tx_deptree.clone();
        thread::spawn(move || {
            let (deps, reqby, root_version) = build_dep_tree(&name);
            let _ = tx.send(UiMessage::DepTreeLoaded { deps, reqby, root_version });
        });
    });

    let ai_enabled_save = appimage_enabled_flag.clone();
    window.on_save_settings(move || {
        if let Some(window) = window_weak_ss.upgrade() {
            let config = build_config(&window);
            xpm_appimage::catalog::set_github_token(Some(config.appimage_github_token.clone()));
            ai_enabled_save.store(config.appimage_enabled, std::sync::atomic::Ordering::Relaxed);
            save_config(&config);
        }
    });

    appimage_enabled_flag.store(config.appimage_enabled, std::sync::atomic::Ordering::Relaxed);
    window.set_setting_flatpak_enabled(config.flatpak_enabled);
    window.set_setting_appimage_enabled(config.appimage_enabled);
    window.set_setting_appimage_dir(SharedString::from(config.appimage_dir.as_str()));
    window.set_setting_appimage_github_token(SharedString::from(config.appimage_github_token.as_str()));
    xpm_appimage::catalog::set_github_token(Some(config.appimage_github_token.clone()));
    *appimage_dir_state.lock().unwrap() = config.appimage_dir.clone();
    let initial_feeds = if config.appimage_feeds.is_empty() {
        default_appimage_feeds()
    } else {
        config.appimage_feeds.clone()
    };
    *appimage_sources_state.lock().unwrap() = initial_feeds.clone();
    window.set_appimage_sources(ModelRc::new(VecModel::from(
        initial_feeds
            .iter()
            .map(|f| AppImageSource {
                name: SharedString::from(f.name.as_str()),
                url: SharedString::from(f.url.as_str()),
            })
            .collect::<Vec<_>>(),
    )));
    window.set_setting_check_updates_on_start(config.check_updates_on_start);
    if config.appimage_enabled {
        let tx_ai_init = tx.clone();
        thread::spawn(move || {
            xpm_appimage::tidy::tidy_store();
            if let Ok(backend) = AppImageBackend::new() {
                let entries = backend.list_entries();
                info!("AppImage startup preload: {} installed", entries.len());
                let _ = tx_ai_init.send(UiMessage::InstalledAppImagesLoaded(entries));
            } else {
                error!("AppImage startup preload: backend init failed");
            }
        });
        let tx_ai_cat_init = tx.clone();
        let cat_init = appimage_catalog.clone();
        let named_init: Vec<(String, String)> =
            initial_feeds.iter().map(|f| (f.name.clone(), f.url.clone())).collect();
        let _ = tx.send(UiMessage::AppImageCatalogLoading(true));
        thread::spawn(move || {
            let entries = xpm_appimage::catalog::fetch_sources_named(&named_init);
            info!("AppImage startup preload: {} catalog entries", entries.len());
            *cat_init.lock().unwrap() = entries;
            let _ = tx_ai_cat_init.send(UiMessage::AppImageCatalogReady);
        });
    } else {
        info!("AppImage startup preload skipped (feature disabled in config)");
    }
    window.set_setting_notify_on_updates(config.notify_on_updates);
    window.set_setting_auto_clean_cache(config.auto_clean_cache);
    let pacman_parallel = read_pacman_parallel_downloads().unwrap_or(config.parallel_downloads);
    window.set_setting_parallel_downloads(pacman_parallel as i32);
    let presets = [5u32, 10, 15, 20, 25];
    if !presets.contains(&pacman_parallel) {
        window.set_setting_pd_custom_mode(true);
        window.set_setting_pd_custom_text(SharedString::from(pacman_parallel.to_string().as_str()));
    }

    window.set_aur_pill_dismissed(config.aur_pill_dismissed);
    window.global::<Cat>().set_font_scale(config.font_scale);

    window.on_save_font_scale(|scale| {
        let mut cfg = load_config();
        cfg.font_scale = scale;
        save_config(&cfg);
    });

    window.on_aur_pill_dismiss(|| {
        let mut cfg = load_config();
        cfg.aur_pill_dismissed = true;
        save_config(&cfg);
    });

    window.on_distro_warning_dismiss(|| {
        let mut cfg = load_config();
        cfg.distro_warning_dismissed = true;
        save_config(&cfg);
    });

    window.window().on_close_requested(|| {
        slint::quit_event_loop().ok();
        slint::CloseRequestResponse::HideWindow
    });

    if !is_xerolinux() && !config.distro_warning_dismissed {
        window.set_show_distro_warning(true);
    }

    info!("Running application");
    window.show().expect("Failed to show window");

    // The .desktop sets StartupNotify=true + StartupWMClass=xpackagemanager so the
    // compositor hands the xdg-activation token to our window (no KWin scripting,
    // which triggered window effects). The delayed activate covers launches where
    // the first activation lands before the surface is configured.
    let focus_weak = window.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
        if let Some(w) = focus_weak.upgrade()
            && !w.window().is_minimized()
        {
            activate_window(&w);
        }
    });
    slint::run_event_loop_until_quit().expect("Failed to run application");
    std::process::exit(0);
}

async fn load_packages_async(tx: &mpsc::Sender<UiMessage>, check_updates: bool) {
    let alpm = match AlpmBackend::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to initialize ALPM: {}", e);
            let _ = tx.send(UiMessage::SetLoading(false));
            return;
        }
    };

    let flatpak = match FlatpakBackend::new() {
        Ok(b) => b,
        Err(e) => {
            error!("Failed to initialize Flatpak: {}", e);
            let _ = tx.send(UiMessage::SetLoading(false));
            return;
        }
    };

    let installed_fut = alpm.list_installed();
    let orphans_fut = alpm.list_orphans();
    let flatpak_installed_fut = flatpak.list_installed();

    let flatpak_updates_fut = if check_updates { Some(flatpak.list_updates()) } else { None };
    let checkupdates_fut = if check_updates {
        Some(tokio::task::spawn_blocking(|| {
            std::process::Command::new("checkupdates")
            .output()
            .or_else(|_| std::process::Command::new("pacman").args(["-Qu"]).output())
        }))
    } else { None };
    let plasmoid_fut = if check_updates { Some(tokio::task::spawn_blocking(list_plasmoids_with_updates)) } else { None };

    let (
        installed_res,
         orphans_res,
         flatpak_installed_res,
    ) = tokio::join!(
        installed_fut,
        orphans_fut,
        flatpak_installed_fut,
    );

    let flatpak_updates = if let Some(fut) = flatpak_updates_fut {
        fut.await.unwrap_or_else(|e| { error!("Failed to list flatpak updates: {}", e); Vec::new() })
    } else { Vec::new() };
    let checkupdates_res = if let Some(fut) = checkupdates_fut { Some(fut.await) } else { None };
    let (_installed_plasmoids, plasmoid_updates) = if let Some(fut) = plasmoid_fut {
        fut.await.unwrap_or_else(|_| (Vec::new(), Vec::new()))
    } else { (Vec::new(), Vec::new()) };
    let installed_pacman = installed_res.unwrap_or_else(|e| { error!("Failed to list installed: {}", e); Vec::new() });
    let orphan_count = orphans_res.map(|o| o.len()).unwrap_or(0);
    let flatpak_packages = flatpak_installed_res.unwrap_or_else(|e| { error!("Failed to list flatpak installed: {}", e); Vec::new() });

    let cache_size = tokio::task::spawn_blocking(|| {
        std::process::Command::new("du")
            .args(["-sb", "/var/cache/pacman/pkg"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().next()
                .and_then(|s| s.parse::<u64>().ok()))
            .unwrap_or(0)
    }).await.unwrap_or(0);

    let mut updates: Vec<xpm_core::package::UpdateInfo> = Vec::new();
    if let Some(Ok(Ok(result))) = checkupdates_res
        && result.status.success() {
            let stdout = String::from_utf8_lossy(&result.stdout);
            for line in stdout.lines() {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    updates.push(xpm_core::package::UpdateInfo {
                        name: parts[0].to_string(),
                                 current_version: xpm_core::package::Version::new(parts[1]),
                                 new_version: xpm_core::package::Version::new(parts[3]),
                                 backend: xpm_core::package::PackageBackend::Pacman,
                                 repository: String::new(),
                                 download_size: 0,
                                 kind: xpm_core::package::UpdateKind::Package,
                    });
                } else if parts.len() >= 2 {
                    updates.push(xpm_core::package::UpdateInfo {
                        name: parts[0].to_string(),
                                 current_version: xpm_core::package::Version::new(""),
                                 new_version: xpm_core::package::Version::new(parts[1]),
                                 backend: xpm_core::package::PackageBackend::Pacman,
                                 repository: String::new(),
                                 download_size: 0,
                                 kind: xpm_core::package::UpdateKind::Package,
                    });
                }
            }
        }

    let update_names: std::collections::HashSet<String> =
    updates.iter().map(|u| u.name.clone()).collect();
    let flatpak_update_names: std::collections::HashSet<String> =
    flatpak_updates.iter().map(|u| u.name.clone()).collect();

    let installed_ui: Vec<PackageData> = installed_pacman
    .iter()
    .map(|p| package_to_ui(p, update_names.contains(&p.name)))
    .collect();

    let updates_ui: Vec<PackageData> = updates.iter().map(update_to_ui).collect();

    let flatpak_ui: Vec<PackageData> = flatpak_packages
    .iter()
    .map(|p| {
        let has_update = flatpak_update_names.contains(&p.name);
        let display_name = if !p.description.is_empty() {
            p.description.clone()
        } else {
            p.name.split('.').next_back().unwrap_or(&p.name)
                .replace(['_', '-'], " ")
        };

        PackageData {
            name: SharedString::from(p.name.as_str()),
         display_name: SharedString::from(&display_name),
         version: SharedString::from(p.version.to_string().as_str()),
         description: SharedString::from(""),
         repository: SharedString::from(p.repository.as_str()),
         backend: 1,
         installed: matches!(
             p.status,
             xpm_core::package::PackageStatus::Installed | xpm_core::package::PackageStatus::Orphan
         ),
         has_update,
         installed_size: SharedString::from(""),
         licenses: SharedString::from(""),
         url: SharedString::from(""),
         dependencies: SharedString::from(""),
         required_by: SharedString::from(""),
         selected: false,
         explicit: false,
        }
    })
    .collect();


    let flatpak_name_map: std::collections::HashMap<String, String> = flatpak_packages
        .iter()
        .map(|p| {
            let display_name = if !p.description.is_empty() {
                p.description.clone()
            } else {
                p.name.split('.').next_back().unwrap_or(&p.name)
                    .replace(['_', '-'], " ")
            };
            (p.name.clone(), display_name)
        })
        .collect();

    let flatpak_updates_ui: Vec<PackageData> = flatpak_updates.iter()
        .map(|u| {
            let display_name = if u.kind == xpm_core::package::UpdateKind::Package {
                flatpak_name_map
                    .get(&u.name)
                    .cloned()
                    .unwrap_or_else(|| {
                        u.name.split('.').next_back().unwrap_or(&u.name)
                            .replace(['_', '-'], " ")
                    })
            } else {
                u.name.clone()
            };
            let ver_str = if u.current_version == u.new_version {
                format!("{} (new build)", u.current_version)
            } else {
                format!("{} → {}", u.current_version, u.new_version)
            };
            PackageData {
                name: SharedString::from(u.name.as_str()),
                display_name: SharedString::from(display_name.as_str()),
                version: SharedString::from(ver_str.as_str()),
                description: SharedString::default(),
                repository: SharedString::from(u.kind.label()),
                backend: 1,
                installed: true,
                has_update: true,
                installed_size: SharedString::from(""),
                licenses: SharedString::from(""),
                url: SharedString::from(""),
                dependencies: SharedString::from(""),
                required_by: SharedString::from(""),
                selected: false,
                explicit: false,
            }
        })
        .collect();

    let mut native_updates_ui = updates_ui.clone();
    native_updates_ui.extend(plasmoid_updates.clone());

    let flatpak_real_count = std::process::Command::new("flatpak")
        .args(["list", "--system"])
        .output()
        .map(|o| o.stdout.iter().filter(|&&b| b == b'\n').count() as i32)
        .unwrap_or(flatpak_packages.len() as i32);

    let stats = StatsData {
        pacman_count: installed_pacman.len() as i32,
        flatpak_count: flatpak_real_count,
        orphan_count: orphan_count as i32,
        cache_size: SharedString::from(format_size(cache_size)),
    };

    let mut all_for_cache = native_updates_ui.clone();
    all_for_cache.extend(flatpak_updates_ui.clone());
    save_package_cache(&installed_ui, &all_for_cache, &flatpak_ui, &stats);

    let _ = tx.send(UiMessage::PackagesLoaded {
        installed: installed_ui,
        updates: native_updates_ui,
        flatpak_updates: flatpak_updates_ui,
        flatpak: flatpak_ui,
        stats,
    });
}

fn list_plasmoids_with_updates() -> (Vec<PackageData>, Vec<PackageData>) {
    let mut plasmoids = Vec::new();
    let mut updates = Vec::new();

    let home = std::env::var("HOME").unwrap_or_default();
    let user_path = std::path::PathBuf::from(&home).join(".local/share/plasma/plasmoids");

    let paths = [
        Some(user_path),
        Some(std::path::PathBuf::from("/usr/share/plasma/plasmoids")),
    ];

    let store_versions = fetch_store_versions();

    for path_opt in paths.iter().flatten() {
        if let Ok(entries) = std::fs::read_dir(path_opt) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let metadata_json = path.join("metadata.json");
                    let metadata_desktop = path.join("metadata.desktop");

                    let info = if metadata_json.exists() {
                        parse_plasmoid_json(&metadata_json)
                    } else if metadata_desktop.exists() {
                        parse_plasmoid_desktop(&metadata_desktop)
                    } else {
                        PlasmoidInfo {
                            id: entry.file_name().to_string_lossy().to_string(),
                            name: entry.file_name().to_string_lossy().to_string(),
                            version: "unknown".to_string(),
                            description: String::new(),
                        }
                    };

                    let is_user = path_opt.to_string_lossy().contains(".local");

                    let (has_update, new_version) = if is_user && !info.name.is_empty() {
                        if let Some((_, store_ver)) = store_versions.iter().find(|(name, _)| name == &info.name) {
                            let is_newer = version_is_newer(store_ver, &info.version);
                            (is_newer, if is_newer { store_ver.clone() } else { String::new() })
                        } else {
                            (false, String::new())
                        }
                    } else {
                        (false, String::new())
                    };

                    let pkg = PackageData {
                        name: SharedString::from(&info.id),
                        display_name: SharedString::from(&info.name),
                        version: SharedString::from(&info.version),
                        description: SharedString::from(&info.description),
                        repository: SharedString::from(if is_user { "kde-store" } else { "system" }),
                        backend: 3,
                        installed: true,
                        has_update,
                        installed_size: SharedString::from(""),
                        licenses: SharedString::from(""),
                        url: SharedString::from(format!("https://store.kde.org/search?search={}", info.name.replace(' ', "+"))),
                        dependencies: SharedString::from(""),
                        required_by: SharedString::from(""),
                        selected: false,
                        explicit: false,
                    };

                    if has_update {
                        let mut update_pkg = pkg.clone();
                        update_pkg.version = SharedString::from(format!("{} → {}", info.version, new_version));
                        updates.push(update_pkg);
                    }

                    plasmoids.push(pkg);
                }
            }
        }
    }

    (plasmoids, updates)
}

fn fetch_store_versions() -> Vec<(String, String)> {
    let mut versions = Vec::new();

    let url = "https://api.kde-look.org/ocs/v1/content/data?categories=705&pagesize=200&format=json";

    if let Ok(output) = std::process::Command::new("curl")
        .args(["-s", "--max-time", "15", url])
        .output()
            && output.status.success() {
                let response = String::from_utf8_lossy(&output.stdout);
                if let Ok(json) = serde_json::from_str::<Value>(&response)
                    && let Some(data) = json.get("ocs").and_then(|o| o.get("data")).and_then(|d| d.as_array()) {
                        for item in data {
                            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let version = item.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            if !name.is_empty() && !version.is_empty() {
                                versions.push((name, version));
                            }
                        }
                    }
            }

        versions
}

fn version_is_newer(store_version: &str, current_version: &str) -> bool {
    let store_parts: Vec<u32> = store_version
    .split(|c: char| !c.is_ascii_digit())
    .filter_map(|s| s.parse().ok())
    .collect();
    let current_parts: Vec<u32> = current_version
    .split(|c: char| !c.is_ascii_digit())
    .filter_map(|s| s.parse().ok())
    .collect();

    for i in 0..store_parts.len().max(current_parts.len()) {
        let store_part = store_parts.get(i).copied().unwrap_or(0);
        let current_part = current_parts.get(i).copied().unwrap_or(0);
        if store_part > current_part {
            return true;
        } else if store_part < current_part {
            return false;
        }
    }
    false
}

struct PlasmoidInfo {
    id: String,
    name: String,
    version: String,
    description: String,
}

fn parse_plasmoid_json(path: &std::path::Path) -> PlasmoidInfo {
    if let Ok(content) = std::fs::read_to_string(path)
        && let Ok(json) = serde_json::from_str::<Value>(&content)
            && let Some(kplugin) = json.get("KPlugin") {
                let id = kplugin.get("Id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
                let name = kplugin.get("Name")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown")
                .to_string();
                let version = kplugin.get("Version")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
                let desc = kplugin.get("Description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
                return PlasmoidInfo { id, name, version, description: desc };
            }
    PlasmoidInfo {
        id: String::new(),
        name: "Unknown".to_string(),
        version: "unknown".to_string(),
        description: String::new(),
    }
}

fn parse_plasmoid_desktop(path: &std::path::Path) -> PlasmoidInfo {
    if let Ok(content) = std::fs::read_to_string(path) {
        let mut id = String::new();
        let mut name = "Unknown".to_string();
        let mut version = "unknown".to_string();
        let mut desc = String::new();

        for line in content.lines() {
            if line.starts_with("Name=") && !line.contains('[') {
                name = line.strip_prefix("Name=").unwrap_or("Unknown").to_string();
            } else if line.starts_with("X-KDE-PluginInfo-Version=") {
                version = line.strip_prefix("X-KDE-PluginInfo-Version=").unwrap_or("unknown").to_string();
            } else if line.starts_with("X-KDE-PluginInfo-Name=") {
                id = line.strip_prefix("X-KDE-PluginInfo-Name=").unwrap_or("").to_string();
            } else if line.starts_with("Comment=") && !line.contains('[') {
                desc = line.strip_prefix("Comment=").unwrap_or("").to_string();
            }
        }
        PlasmoidInfo { id, name, version, description: desc }
    } else {
        PlasmoidInfo {
            id: String::new(),
            name: "Unknown".to_string(),
            version: "unknown".to_string(),
            description: String::new(),
        }
    }
}


/// Relevance tier for a search hit (lower = better, None = no match). `q` must be
/// lowercased; `id` is the flatpak app-id (pass name again for native pkgs).
/// Priority: name > id > description, exact > prefix > substring.
fn search_rank(q: &str, name: &str, id: &str, desc: &str) -> Option<u8> {
    let name = name.to_lowercase();
    let id = id.to_lowercase();
    let desc = desc.to_lowercase();
    if name == q { Some(0) }
    else if name.starts_with(q) { Some(1) }
    else if id == q { Some(2) }
    else if name.contains(q) { Some(3) }
    else if id.starts_with(q) { Some(4) }
    else if id.contains(q) { Some(5) }
    else if desc.contains(q) { Some(6) }
    else { None }
}

async fn search_packages_async(
    tx: &mpsc::Sender<UiMessage>,
    query: &str,
    flatpak_store: Arc<Mutex<Vec<CachedRemoteApp>>>,
    flatpak_ids: Arc<Mutex<std::collections::HashSet<String>>>,
) {
    let q = query.to_string();
    let q_lower = q.to_lowercase();

    let (store_snapshot, ids_snapshot) = {
        let store = flatpak_store.lock().unwrap();
        let ids = flatpak_ids.lock().unwrap();
        (store.clone(), ids.clone())
    };
    let store_is_empty = store_snapshot.is_empty();

    let alpm_query = q.clone();
    let alpm_future = async move {
        let alpm = AlpmBackend::new().ok()?;
        alpm.search(&alpm_query).await.ok()
    };

    let fk_future = tokio::task::spawn_blocking(move || -> (Vec<CachedRemoteApp>, std::collections::HashSet<String>) {
        if store_is_empty {
            (fetch_remote_apps_cached("flathub"), get_flatpak_installed_ids())
        } else {
            (store_snapshot, ids_snapshot)
        }
    });

    let (alpm_result, fk_result) = tokio::join!(alpm_future, fk_future);

    let pacman_results = alpm_result.unwrap_or_default();
    let (flatpak_apps, flatpak_installed) = fk_result.unwrap_or_default();

    // Score every candidate (native + flatpak) on one scale, then sort by
    // (relevance, backend) so that at equal relevance native packages rank ahead
    // of flatpaks. Flatpaks are ranked too (was previously an arbitrary take(50)).
    let mut scored: Vec<(u8, u8, PackageData)> = Vec::new();

    for r in &pacman_results {
        if let Some(rank) = search_rank(&q_lower, &r.name, &r.name, &r.description) {
            scored.push((rank, 0, PackageData {
                name: SharedString::from(r.name.as_str()),
                display_name: SharedString::from(r.name.as_str()),
                version: SharedString::from(r.version.to_string().as_str()),
                description: SharedString::from(r.description.as_str()),
                repository: SharedString::from(r.repository.as_str()),
                backend: 0,
                installed: r.installed,
                has_update: false,
                installed_size: SharedString::from(""),
                licenses: SharedString::from(""),
                url: SharedString::from(""),
                dependencies: SharedString::from(""),
                required_by: SharedString::from(""),
                selected: false,
                explicit: false,
            }));
        }
    }

    for a in &flatpak_apps {
        if let Some(rank) = search_rank(&q_lower, &a.name, &a.app_id, &a.summary) {
            scored.push((rank, 1, PackageData {
                name: SharedString::from(a.app_id.as_str()),
                display_name: SharedString::from(if a.name.is_empty() { &a.app_id } else { &a.name }),
                version: SharedString::from(a.version.as_str()),
                description: SharedString::from(a.summary.as_str()),
                repository: SharedString::from("Flatpak"),
                backend: 1,
                installed: flatpak_installed.contains(&a.app_id),
                has_update: false,
                installed_size: SharedString::from(""),
                licenses: SharedString::from(""),
                url: SharedString::from(""),
                dependencies: SharedString::from(""),
                required_by: SharedString::from(""),
                selected: false,
                explicit: false,
            }));
        }
    }

    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let results: Vec<PackageData> = scored.into_iter().map(|(_, _, p)| p).take(200).collect();
    let _ = tx.send(UiMessage::SearchResults(results));
}


#[derive(Serialize, Deserialize, Clone)]
struct CachedPkg {
    name: String,
    display_name: String,
    version: String,
    description: String,
    repository: String,
    backend: i32,
    installed: bool,
    has_update: bool,
    installed_size: String,
}

#[derive(Serialize, Deserialize)]
struct PackageCache {
    pacman_db_mtime: u64,
    installed: Vec<CachedPkg>,
    updates: Vec<CachedPkg>,
    flatpak: Vec<CachedPkg>,
    pacman_count: i32,
    flatpak_count: i32,
    orphan_count: i32,
    cache_size: String,
}

fn pkg_cache_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(format!("{}/.local/share/xpm/pkg_cache.json", home))
}

fn pacman_db_mtime() -> u64 {
    std::fs::metadata("/var/lib/pacman/local")
        .and_then(|m| m.modified())
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
        .unwrap_or(0)
}

fn pkg_to_cached(p: &PackageData) -> CachedPkg {
    CachedPkg {
        name: p.name.to_string(),
        display_name: p.display_name.to_string(),
        version: p.version.to_string(),
        description: p.description.to_string(),
        repository: p.repository.to_string(),
        backend: p.backend,
        installed: p.installed,
        has_update: p.has_update,
        installed_size: p.installed_size.to_string(),
    }
}

fn cached_to_pkg(c: &CachedPkg) -> PackageData {
    PackageData {
        name: SharedString::from(c.name.as_str()),
        display_name: SharedString::from(c.display_name.as_str()),
        version: SharedString::from(c.version.as_str()),
        description: SharedString::from(c.description.as_str()),
        repository: SharedString::from(c.repository.as_str()),
        backend: c.backend,
        installed: c.installed,
        has_update: c.has_update,
        installed_size: SharedString::from(c.installed_size.as_str()),
        licenses: SharedString::from(""),
        url: SharedString::from(""),
        dependencies: SharedString::from(""),
        required_by: SharedString::from(""),
        selected: false,
        explicit: false,
    }
}

fn save_package_cache(installed: &[PackageData], updates: &[PackageData], flatpak: &[PackageData], stats: &StatsData) {
    let cache = PackageCache {
        pacman_db_mtime: pacman_db_mtime(),
        installed: installed.iter().map(pkg_to_cached).collect(),
        updates: updates.iter().map(pkg_to_cached).collect(),
        flatpak: flatpak.iter().map(pkg_to_cached).collect(),
        pacman_count: stats.pacman_count,
        flatpak_count: stats.flatpak_count,
        orphan_count: stats.orphan_count,
        cache_size: stats.cache_size.to_string(),
    };
    let path = pkg_cache_path();
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = std::fs::write(&path, json);
    }
}

fn load_package_cache() -> Option<PackageCache> {
    let path = pkg_cache_path();
    let content = std::fs::read_to_string(&path).ok()?;
    let cache: PackageCache = serde_json::from_str(&content).ok()?;
    if cache.pacman_db_mtime == pacman_db_mtime() {
        Some(cache)
    } else {
        None
    }
}


fn remote_cache_path(remote: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(format!("{}/.local/share/xpm/remote_{}.v2.json", home, remote))
}

fn remote_cache_valid(path: &std::path::Path) -> bool {
    if let Ok(meta) = std::fs::metadata(path)
        && let Ok(modified) = meta.modified() {
            let age = std::time::SystemTime::now()
                .duration_since(modified)
                .unwrap_or(std::time::Duration::MAX);
            return age.as_secs() < 86400;
        }
    false
}


fn fetch_flatpak_remotes() -> Vec<String> {
    let Ok(out) = std::process::Command::new("flatpak")
        .args(["remotes", "--columns=name"])
        .output() else { return Vec::new(); };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && l.trim() != "Name")
        .map(|l| l.trim().to_string())
        .collect()
}

fn get_flatpak_installed_ids() -> std::collections::HashSet<String> {
    match std::process::Command::new("flatpak")
        .args(["list", "--columns=application"])
        .output()
    {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        Err(_) => std::collections::HashSet::new(),
    }
}

/// Active appstream directory for a remote, preferring the user installation
/// (~/.local/share/flatpak) over the system one (/var/lib/flatpak). Falls back to
/// the system path even when nothing is cached yet (so callers get a sane path).
fn appstream_base(remote: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        let user = format!("{}/.local/share/flatpak/appstream/{}/x86_64/active", home, remote);
        if std::path::Path::new(&format!("{}/appstream.xml", user)).exists()
            || std::path::Path::new(&format!("{}/appstream.xml.gz", user)).exists()
        {
            return user;
        }
    }
    format!("/var/lib/flatpak/appstream/{}/x86_64/active", remote)
}

fn parse_appstream_xml(remote: &str) -> Vec<CachedRemoteApp> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let base = appstream_base(remote);
    let xml_path = format!("{}/appstream.xml", base);
    let gz_path = format!("{}.gz", xml_path);

    let xml_bytes: Vec<u8> = if std::path::Path::new(&xml_path).exists() {
        match std::fs::read(&xml_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[xpm] parse_appstream_xml: failed to read {}: {}", xml_path, e);
                return Vec::new();
            }
        }
    } else if std::path::Path::new(&gz_path).exists() {
        match std::fs::File::open(&gz_path) {
            Ok(f) => {
                let mut dec = GzDecoder::new(f);
                let mut bytes = Vec::new();
                if let Err(e) = dec.read_to_end(&mut bytes) {
                    eprintln!("[xpm] parse_appstream_xml: failed to decompress {}: {}", gz_path, e);
                    return Vec::new();
                }
                bytes
            }
            Err(e) => {
                eprintln!("[xpm] parse_appstream_xml: failed to open {}: {}", gz_path, e);
                return Vec::new();
            }
        }
    } else {
        eprintln!("[xpm] parse_appstream_xml: appstream data not found at {} or {}", xml_path, gz_path);
        eprintln!("[xpm] hint: run 'flatpak update' to populate the appstream cache");
        return Vec::new();
    };

    crate::appstream::parse(&xml_bytes)
}

fn fetch_remote_apps_cached(remote: &str) -> Vec<CachedRemoteApp> {
    let path = remote_cache_path(remote);
    if remote_cache_valid(&path)
        && let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(apps) = serde_json::from_str::<Vec<CachedRemoteApp>>(&content) {
                return apps;
            }

    let apps = parse_appstream_xml(remote);

    if !apps.is_empty() {
        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Ok(json) = serde_json::to_string(&apps) {
            let _ = std::fs::write(&path, json);
        }
        let _ = std::fs::remove_file(path.with_file_name(format!("remote_{remote}.json")));
    }

    apps
}

fn apps_to_package_data(
    apps: &[CachedRemoteApp],
    installed_ids: &std::collections::HashSet<String>,
    remote: &str,
    category_filter: &str,
    search: &str,
) -> Vec<PackageData> {
    let has_addons: std::collections::HashSet<&str> = apps.iter()
        .filter(|a| !a.extends.is_empty())
        .map(|a| a.extends.as_str())
        .collect();

    let search_lower = search.to_lowercase();
    let icon_base = appstream_base(remote);
    apps.iter()
        .filter(|app| {
            if !category_filter.is_empty() && category_filter != "All"
                && !app.categories.iter().any(|c| c == category_filter) {
                    return false;
                }
            if !search_lower.is_empty() {
                let name_lower = app.name.to_lowercase();
                let id_lower = app.app_id.to_lowercase();
                let sum_lower = app.summary.to_lowercase();
                if !name_lower.contains(&search_lower)
                    && !id_lower.contains(&search_lower)
                    && !sum_lower.contains(&search_lower)
                {
                    return false;
                }
            }
            true
        })
        .map(|app| {
            let icon_path = if !app.icon_name.is_empty() {
                format!("{}/icons/128x128/{}", icon_base, app.icon_name)
            } else {
                String::new()
            };
            let initial = app.name.chars()
                .next()
                .or_else(|| app.app_id.chars().next())
                .map(|c| c.to_uppercase().next().unwrap_or(c))
                .map(|c| c.to_string())
                .unwrap_or_default();
            let primary_cat = app.categories.first().cloned().unwrap_or_default();
            PackageData {
                name: SharedString::from(app.app_id.as_str()),
                display_name: SharedString::from(if app.name.is_empty() { &app.app_id } else { &app.name }),
                version: SharedString::from(""),
                description: SharedString::from(app.summary.as_str()),
                repository: SharedString::from(remote),
                backend: 1,
                installed: installed_ids.contains(&app.app_id),
                has_update: false,
                installed_size: SharedString::from(primary_cat.as_str()),
                licenses: SharedString::from(icon_path.as_str()),
                url: SharedString::from(app.screenshot_url.as_str()),
                dependencies: SharedString::from(app.developer.as_str()),
                required_by: SharedString::from(initial.as_str()),
                selected: false,
                explicit: has_addons.contains(app.app_id.as_str()),
            }
        })
        .collect()
}

fn load_remote_apps(remote: &str) -> (Vec<CachedRemoteApp>, std::collections::HashSet<String>) {
    let apps = fetch_remote_apps_cached(remote);
    let installed = get_flatpak_installed_ids();
    (apps, installed)
}


fn load_pacman_repos() -> Vec<String> {
    let out = std::process::Command::new("pacman")
        .args(["-Sl"])
        .output();
    match out {
        Ok(o) => {
            let stdout = String::from_utf8_lossy(&o.stdout).into_owned();
            stdout.lines()
                .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        }
        Err(_) => Vec::new(),
    }
}

fn load_repo_descriptions(repo: &str) -> std::collections::HashMap<String, String> {
    let out = std::process::Command::new("expac")
        .args(["-S", "%r\t%n\t%d"])
        .output();
    if let Ok(o) = out
        && o.status.success() {
            return String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(3, '\t');
                    let r = parts.next()?;
                    let n = parts.next()?;
                    let d = parts.next().unwrap_or("").trim();
                    if (repo.is_empty() || r == repo) && !d.is_empty() {
                        Some((n.to_string(), d.to_string()))
                    } else {
                        None
                    }
                })
                .collect();
        }
    std::collections::HashMap::new()
}

fn load_repo_packages(repo: &str) -> Vec<PackageData> {
    let desc_map = load_repo_descriptions(repo);
    let mut cmd = std::process::Command::new("pacman");
    cmd.arg("-Sl");
    if !repo.is_empty() { cmd.arg(repo); }
    let out = cmd.output();
    match out {
        Ok(o) => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() < 3 { return None; }
                    let repo_name = parts[0];
                    let name = parts[1];
                    let version = parts[2];
                    let installed = parts.get(3).is_some_and(|s| *s == "[installed]");
                    let description = desc_map.get(name).cloned().unwrap_or_default();
                    Some(PackageData {
                        name: SharedString::from(name),
                        display_name: SharedString::from(name),
                        version: SharedString::from(version),
                        description: SharedString::from(&description),
                        repository: SharedString::from(repo_name),
                        backend: 0,
                        installed,
                        has_update: false,
                        installed_size: SharedString::from(""),
                        licenses: SharedString::from(""),
                        url: SharedString::from(""),
                        dependencies: SharedString::from(""),
                        required_by: SharedString::from(""),
                        selected: false,
                        explicit: false,
                    })
                })
                .collect()
        }
        Err(_) => Vec::new(),
    }
}


#[cfg(test)]
mod search_tests {
    use super::search_rank;

    #[test]
    fn exact_name_beats_prefix_beats_substring() {
        // q must be lowercase (caller lowercases it).
        let exact = search_rank("gimp", "gimp", "org.gimp.GIMP", "image editor");
        let prefix = search_rank("gim", "gimp", "org.gimp.GIMP", "image editor");
        let substr = search_rank("imp", "gimp", "org.gimp.GIMP", "image editor");
        assert_eq!(exact, Some(0));
        assert_eq!(prefix, Some(1));
        assert!(substr > Some(1));
    }

    #[test]
    fn id_and_description_match_rank_lower_than_name() {
        // Match only via app-id.
        let id = search_rank("gimp", "Photo Tool", "org.gimp.GIMP", "editor");
        assert!(id.is_some());
        // Match only via description ranks worst.
        let desc = search_rank("editor", "Photo Tool", "org.gimp.GIMP", "an image editor");
        assert_eq!(desc, Some(6));
        assert!(id < desc);
    }

    #[test]
    fn no_match_is_none() {
        assert_eq!(search_rank("xyzzy", "gimp", "org.gimp.GIMP", "image editor"), None);
    }
}

#[cfg(test)]
mod news_tests {
    use super::*;

    #[test]
    fn parses_items_with_entities_and_cdata() {
        let xml = r#"<?xml version="1.0"?><rss><channel><title>feed</title>
<item><title>Tom &amp; Jerry update</title>
<link>https://archlinux.org/news/a/</link>
<pubDate>Mon, 05 Jan 2026 10:00:00 +0000</pubDate>
<description>&lt;p&gt;Manual &amp;amp; intervention&lt;/p&gt;</description></item>
<item><title>Second</title><link>https://archlinux.org/news/b/</link>
<pubDate>Tue, 06 Jan 2026 10:00:00 +0000</pubDate>
<description><![CDATA[<p>Plain</p>]]></description></item>
</channel></rss>"#;
        let items = parse_arch_rss(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Tom & Jerry update");
        assert_eq!(items[0].date, "05 Jan 2026");
        assert_eq!(items[1].summary, "Plain");
    }
}

#[cfg(test)]
mod update_plan_tests {
    use super::update_script;

    #[test]
    fn script_matches_enabled_backends() {
        assert_eq!(update_script(false, false), None);
        assert_eq!(update_script(true, false).as_deref(), Some("pkexec pacman -Syu"));
        let both = update_script(true, true).unwrap();
        assert!(both.starts_with("pkexec bash -c 'pacman -Syu"));
        assert!(both.contains("flatpak update --user"));
        let flatpak_only = update_script(false, true).unwrap();
        assert!(!flatpak_only.contains("pacman"));
        assert!(flatpak_only.contains("pkexec flatpak update --system"));
    }
}

#[cfg(test)]
mod flatpak_file_tests {
    use super::*;

    #[test]
    fn detects_flatpak_file_kinds() {
        assert!(flatpak_file_kind("/a/b.flatpak") == Some(FlatpakFile::Install));
        assert!(flatpak_file_kind("/a/b.FlatpakRef") == Some(FlatpakFile::Install));
        assert!(flatpak_file_kind("/a/b.flatpakrepo") == Some(FlatpakFile::Repo));
        assert!(flatpak_file_kind("/a/b.AppImage").is_none());
    }

    #[test]
    fn parses_flatpakrepo() {
        let dir = std::env::temp_dir().join(format!("xpm-repo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("my repo.flatpakrepo");
        std::fs::write(&path, "[Flatpak Repo]\nTitle=My Repo\nUrl=https://example.org/repo/\n").unwrap();
        let repo = parse_flatpakrepo(path.to_str().unwrap()).unwrap();
        assert_eq!(repo.name, "my-repo");
        assert_eq!(repo.title, "My Repo");
        assert_eq!(repo.url, "https://example.org/repo/");
        std::fs::write(&path, "[Flatpak Repo]\nTitle=No Url\n").unwrap();
        assert!(parse_flatpakrepo(path.to_str().unwrap()).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
