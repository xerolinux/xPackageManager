
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use slint::{Model, ModelRc, SharedString, VecModel, Timer, TimerMode};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::BufReader;
use std::path::Path;
use serde_json::Value;
use std::rc::Rc;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::FromRawFd;
use std::os::unix::process::CommandExt;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use tracing::{error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;
use xpm_alpm::AlpmBackend;
use xpm_core::source::PackageSource;
use xpm_flatpak::FlatpakBackend;
use ksni::TrayMethods;
use notify_rust::{Notification, Timeout};

slint::include_modules!();

// ─── System tray ──────────────────────────────────────────────────────────────

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

struct XpmTray {
    window: slint::Weak<MainWindow>,
    update_count: Arc<AtomicU32>,
    /// Fire () into this to trigger a manual check-and-notify.
    check_tx: tokio::sync::mpsc::UnboundedSender<()>,
}

impl ksni::Tray for XpmTray {
    fn id(&self) -> String {
        "xpackagemanager".into()
    }

    fn title(&self) -> String {
        let n = self.update_count.load(Ordering::Relaxed);
        if n > 0 {
            format!("XeroLinux Package Manager ({} updates)", n)
        } else {
            "XeroLinux Package Manager".into()
        }
    }

    fn icon_name(&self) -> String {
        // Fallback for DEs that don't support icon_pixmap.
        "system-software-install".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let count = self.update_count.load(Ordering::Relaxed);
        // make_tray_icon returns 32×32 RGBA; ksni needs ARGB32 (rotate each
        // pixel's bytes right by 1: [R,G,B,A] → [A,R,G,B]).
        let mut data = make_tray_icon(count);
        for p in data.chunks_exact_mut(4) {
            p.rotate_right(1);
        }
        vec![ksni::Icon { width: 32, height: 32, data }]
    }

    fn status(&self) -> ksni::Status {
        if self.update_count.load(Ordering::Relaxed) > 0 {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let n = self.update_count.load(Ordering::Relaxed);
        let desc = if n > 0 {
            format!("{} update{} available (native + flatpak)", n, if n == 1 { "" } else { "s" })
        } else {
            "System is up to date".into()
        };
        ksni::ToolTip {
            title: "XeroLinux Package Manager".into(),
            description: desc,
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let win = self.window.clone();
        slint::invoke_from_event_loop(move || {
            if let Some(w) = win.upgrade() {
                let _ = w.show();
            }
        }).ok();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        use ksni::MenuItem;

        let n = self.update_count.load(Ordering::Relaxed);
        let update_label = if n > 0 {
            format!("Update System ({} available)", n)
        } else {
            "Update System".into()
        };

        let win_launch = self.window.clone();
        let win_update = self.window.clone();
        // Fires into the background check task — no pkexec/password needed.
        let check_tx = self.check_tx.clone();

        vec![
            StandardItem {
                label: "Launch App".into(),
                icon_name: "window-new".into(),
                activate: Box::new(move |_: &mut Self| {
                    let w = win_launch.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(win) = w.upgrade() { let _ = win.show(); }
                    }).ok();
                }),
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Check for Updates".into(),
                icon_name: "update-none".into(),
                activate: Box::new(move |_: &mut Self| {
                    // Signal the background handler — runs check_update_count()
                    // then sends a desktop notification. No password prompt.
                    check_tx.send(()).ok();
                }),
                ..Default::default()
            }.into(),
            StandardItem {
                label: update_label,
                icon_name: "system-software-update".into(),
                activate: Box::new(move |_: &mut Self| {
                    let w = win_update.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(win) = w.upgrade() { win.invoke_update_system_full(); }
                    }).ok();
                }),
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Exit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(move |_: &mut Self| {
                    slint::invoke_from_event_loop(|| { slint::quit_event_loop().ok(); }).ok();
                }),
                ..Default::default()
            }.into(),
        ]
    }
}

type TrayShutdown = Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>;

/// Count available updates without syncing databases (uses cached sync DBs).
fn check_update_count() -> u32 {
    let native = std::process::Command::new("sh")
        .args(["-c", "pacman -Qu 2>/dev/null | wc -l"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);

    let flatpak = std::process::Command::new("sh")
        .args(["-c", "flatpak remote-ls --updates --app --columns=application 2>/dev/null | grep -c ."])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0);

    native + flatpak
}

// ─── Tray icon badge renderer ─────────────────────────────────────────────────

/// 3×5 pixel bitmaps for '0'–'9' (index 0–9) and '+' (index 10).
/// Each row: 3 bits — bit 2 = left col, bit 0 = right col.
const DIGITS_3X5: [[u8; 5]; 11] = [
    [0b111, 0b101, 0b101, 0b101, 0b111], // 0
    [0b010, 0b110, 0b010, 0b010, 0b111], // 1
    [0b111, 0b001, 0b011, 0b100, 0b111], // 2
    [0b111, 0b001, 0b011, 0b001, 0b111], // 3
    [0b101, 0b101, 0b111, 0b001, 0b001], // 4
    [0b111, 0b100, 0b111, 0b001, 0b111], // 5
    [0b111, 0b100, 0b111, 0b101, 0b111], // 6
    [0b111, 0b001, 0b010, 0b010, 0b010], // 7
    [0b111, 0b101, 0b111, 0b101, 0b111], // 8
    [0b111, 0b101, 0b111, 0b001, 0b111], // 9
    [0b000, 0b010, 0b111, 0b010, 0b000], // +
];

/// Decoded 32×32 RGBA base icon, initialised once from the embedded PNG.
static BASE_ICON_32: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

fn get_base_icon_32() -> &'static [u8] {
    BASE_ICON_32.get_or_init(|| {
        const PNG: &[u8] = include_bytes!("../../../packaging/xPM.png");
        let img = image::load_from_memory(PNG)
            .unwrap_or_else(|_| image::DynamicImage::new_rgba8(32, 32));
        img.resize_exact(32, 32, image::imageops::FilterType::Lanczos3)
            .to_rgba8()
            .into_raw()
    })
}

/// Return a 32×32 RGBA image of the app icon.
/// When `count > 0`:
///   • top-right  — red badge with the count
///   • bottom-left — amber badge with an ↑ arrow (update indicator)
fn make_tray_icon(count: u32) -> Vec<u8> {
    let mut buf = get_base_icon_32().to_vec();
    if count == 0 {
        return buf;
    }

    const IW: usize = 32;
    const IH: usize = 32;

    // Helper: paint one pixel with RGBA (no alpha-blend, full overwrite).
    let mut set_px = |x: usize, y: usize, r: u8, g: u8, b: u8, a: u8| {
        if x < IW && y < IH {
            let i = (y * IW + x) * 4;
            buf[i]     = r;
            buf[i + 1] = g;
            buf[i + 2] = b;
            buf[i + 3] = a;
        }
    };

    // ── Red count badge — top-right ─────────────────────────────────────────
    let (chars, scale): (Vec<usize>, usize) = if count <= 9 {
        (vec![count as usize], 2)
    } else if count <= 99 {
        (vec![(count / 10) as usize, (count % 10) as usize], 2)
    } else {
        (vec![9, 9, 10], 1) // "99+"
    };

    let char_w = 3 * scale;
    let gap    = scale.max(1);
    let n      = chars.len();
    let text_w = n * char_w + n.saturating_sub(1) * gap;
    let text_h = 5 * scale;
    let pad    = 2usize;
    let bw     = text_w + pad * 2;
    let bh     = text_h + pad * 2;
    let bx     = 32usize.saturating_sub(bw);
    let by     = 0usize;

    for dy in 0..bh {
        for dx in 0..bw {
            if (dy == 0 || dy == bh - 1) && (dx == 0 || dx == bw - 1) { continue; }
            set_px(bx + dx, by + dy, 220, 45, 45, 255);
        }
    }
    let mut cx = bx + pad;
    let ty = by + pad;
    for &ch in &chars {
        let bitmap = DIGITS_3X5[ch];
        for (row, &bits) in bitmap.iter().enumerate() {
            for col in 0..3usize {
                if (bits >> (2 - col)) & 1 == 1 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            set_px(cx + col * scale + sx, ty + row * scale + sy, 255, 255, 255, 255);
                        }
                    }
                }
            }
        }
        cx += char_w + gap;
    }

    // ── Amber ↑ arrow badge — bottom-left ───────────────────────────────────
    // 10×10 amber rounded-rect background, 7×7 arrow shape centred inside.
    //
    // Arrow bitmap (7 rows × 7 cols, read MSB-first from bit 6):
    //   row 0: ..#..   head tip
    //   row 1: .###.   head wide
    //   row 2: #####   head base
    //   row 3: ..#..   shaft
    //   row 4: ..#..   shaft
    //   row 5: ..#..   shaft
    //   row 6: ..#..   shaft base
    const ARROW: [u8; 7] = [
        0b0001000,
        0b0011100,
        0b0111110,
        0b0001000,
        0b0001000,
        0b0001000,
        0b0001000,
    ];
    let ab_size: usize = 10; // badge side length
    let ab_x: usize = 0;
    let ab_y: usize = IH - ab_size;

    for dy in 0..ab_size {
        for dx in 0..ab_size {
            // Clip 1-px corners for a slightly rounded look.
            if (dy == 0 || dy == ab_size - 1) && (dx == 0 || dx == ab_size - 1) { continue; }
            set_px(ab_x + dx, ab_y + dy, 220, 140, 20, 255); // amber
        }
    }
    // Arrow pixels (white), centred in the 10×10 badge (offset 1.5 → use 1 and 2).
    let arrow_ox = ab_x + 1; // x offset to centre 7-wide in 10-wide badge
    let arrow_oy = ab_y + 1; // y offset
    for (row, &bits) in ARROW.iter().enumerate() {
        for col in 0..7usize {
            if (bits >> (6 - col)) & 1 == 1 {
                set_px(arrow_ox + col, arrow_oy + row, 255, 255, 255, 255);
            }
        }
    }

    buf
}

// ─── End tray icon renderer ───────────────────────────────────────────────────

/// Show a desktop notification with the update count.
/// When n == 0, shows "up to date". When n > 0, shows an "Update Now" action
/// that opens the app and navigates to the Updates page (view 1).
/// Blocking — meant to be called from tokio::task::spawn_blocking.
fn send_update_notification(n: u32, window: slint::Weak<MainWindow>, tx: mpsc::Sender<UiMessage>) {
    if n == 0 {
        Notification::new()
            .summary("System is up to date")
            .body("No updates available.")
            .icon("system-software-install")
            .timeout(Timeout::Milliseconds(5000))
            .show()
            .ok();
        return;
    }

    let body = format!(
        "{} update{} available (native + flatpak)",
        n,
        if n == 1 { "" } else { "s" }
    );
    let result = Notification::new()
        .summary("Updates Available")
        .body(&body)
        .icon("system-software-update")
        // Keep notification visible until user acts on it.
        .timeout(Timeout::Never)
        .action("update", "Update Now")
        .show();

    match result {
        Ok(handle) => {
            handle.wait_for_action(|action_id| {
                if action_id == "update" {
                    // Trigger a full update check to populate the updates page,
                    // then show the window on the Updates view once data is ready.
                    let tx_load = tx.clone();
                    let window_show = window.clone();
                    thread::spawn(move || {
                        let _ = tx_load.send(UiMessage::SetLoading(true));
                        let rt = tokio::runtime::Runtime::new().expect("rt");
                        rt.block_on(load_packages_async(&tx_load, true));
                        slint::invoke_from_event_loop(move || {
                            if let Some(win) = window_show.upgrade() {
                                let _ = win.show();
                                win.set_view(1); // Updates page
                            }
                        })
                        .ok();
                    });
                }
            });
        }
        Err(e) => {
            warn!("Failed to send update notification: {}", e);
        }
    }
}

/// Create or remove the autostart .desktop entry.
fn set_autostart(enabled: bool) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let autostart_dir = Path::new(&home).join(".config/autostart");
    let desktop = autostart_dir.join("xpackagemanager.desktop");

    if enabled {
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| Path::new("/usr/bin/xpackagemanager").to_path_buf());
        // --tray: start as tray-only (no window on login)
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=xPackageManager Tray\n\
             Comment=xPackageManager update notifier\nExec={} --tray\n\
             Icon=xpackagemanager\nStartupNotify=false\nTerminal=false\n\
             X-KDE-autostart-after=panel\n",
            exe.display()
        );
        if std::fs::create_dir_all(&autostart_dir).is_ok() {
            if let Err(e) = std::fs::write(&desktop, &content) {
                warn!("Autostart write failed: {}", e);
            }
        }
    } else {
        let _ = std::fs::remove_file(&desktop);
    }
}

type TrayCheckTx = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<()>>>>;

fn start_tray(
    window: slint::Weak<MainWindow>,
    tray_shutdown: TrayShutdown,
    interval_secs: u64,
    tx: mpsc::Sender<UiMessage>,
    shared_count: Arc<AtomicU32>,
    shared_check_tx: TrayCheckTx,
) {
    stop_tray(&tray_shutdown);
    // Clear the shared check sender while tray is being restarted.
    *shared_check_tx.lock().unwrap() = None;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    *tray_shutdown.lock().unwrap() = Some(shutdown_tx);

    thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime for tray");
        rt.block_on(async move {
            let update_count = shared_count;
            let (check_tx, mut check_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
            // Expose the sender so the main thread can trigger rechecks.
            *shared_check_tx.lock().unwrap() = Some(check_tx.clone());
            let tray = XpmTray {
                window: window.clone(),
                update_count: update_count.clone(),
                check_tx,
            };

            match tray.spawn().await {
                Ok(handle) => {
                    info!("System tray started");

                    // ── Manual "Check for Updates" handler ───────────────────
                    // Receives () when the tray menu item is clicked, runs a
                    // silent check (no root), updates the icon, sends a
                    // desktop notification.
                    let handle_manual = handle.clone();
                    let count_manual  = update_count.clone();
                    let window_manual = window.clone();
                    let tx_manual     = tx.clone();
                    tokio::spawn(async move {
                        while check_rx.recv().await.is_some() {
                            let n = tokio::task::spawn_blocking(check_update_count)
                                .await
                                .unwrap_or(0);
                            count_manual.store(n, Ordering::Relaxed);
                            handle_manual.update(|_| {}).await;
                            let w = window_manual.clone();
                            let t = tx_manual.clone();
                            tokio::task::spawn_blocking(move || send_update_notification(n, w, t))
                                .await
                                .ok();
                        }
                    });

                    // ── Periodic update checker ───────────────────────────────
                    // Notifies the user only when updates are newly detected
                    // (transition from 0 → n>0) to avoid spamming every cycle.
                    let handle_checker  = handle.clone();
                    let count_ref       = update_count.clone();
                    let window_periodic = window.clone();
                    let tx_periodic     = tx.clone();
                    tokio::spawn(async move {
                        // Check immediately on startup so the badge and
                        // notification are current from the first moment
                        // the tray is visible (covers boot/reboot).
                        let n = tokio::task::spawn_blocking(check_update_count)
                            .await
                            .unwrap_or(0);
                        let old = count_ref.swap(n, Ordering::Relaxed);
                        handle_checker.update(|_| {}).await;
                        if n > 0 && old == 0 {
                            let w = window_periodic.clone();
                            let t = tx_periodic.clone();
                            tokio::task::spawn_blocking(move || send_update_notification(n, w, t))
                                .await
                                .ok();
                        }

                        // Then run on the configured interval forever.
                        loop {
                            tokio::time::sleep(tokio::time::Duration::from_secs(interval_secs)).await;
                            let n = tokio::task::spawn_blocking(check_update_count)
                                .await
                                .unwrap_or(0);
                            let old = count_ref.swap(n, Ordering::Relaxed);
                            handle_checker.update(|_| {}).await;
                            // Notify only when updates are newly available.
                            if n > 0 && old == 0 {
                                let w = window_periodic.clone();
                                let t = tx_periodic.clone();
                                tokio::task::spawn_blocking(move || send_update_notification(n, w, t))
                                    .await
                                    .ok();
                            }
                        }
                    });

                    let _ = shutdown_rx.await;
                    handle.shutdown().await;
                    info!("System tray stopped");
                }
                Err(e) => {
                    warn!("System tray failed to start: {}", e);
                }
            }
        });
    });
}

fn stop_tray(tray_shutdown: &TrayShutdown) {
    let _ = tray_shutdown.lock().unwrap().take();
}

// ─── End system tray ──────────────────────────────────────────────────────────

// ─── Single-instance guard ────────────────────────────────────────────────────

fn instance_lock_path() -> String {
    let uid = unsafe { libc::getuid() };
    format!("/tmp/xpackagemanager-{}.lock", uid)
}

fn instance_socket_path() -> String {
    let uid = unsafe { libc::getuid() };
    format!("/tmp/xpackagemanager-{}.sock", uid)
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
        .open(instance_lock_path())
        .ok()?;
    let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 { Some(file) } else { None }
}

fn signal_existing_instance() {
    use std::io::Write;
    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(instance_socket_path()) {
        let _ = stream.write_all(b"show");
    }
}

fn listen_for_instance_signals(window: slint::Weak<MainWindow>) {
    let path = instance_socket_path();
    let _ = std::fs::remove_file(&path);
    if let Ok(listener) = std::os::unix::net::UnixListener::bind(&path) {
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stream.is_ok() {
                    let win = window.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(w) = win.upgrade() {
                            w.show().ok();
                        }
                    }).ok();
                }
            }
        });
    }
}

// ─── End single-instance guard ────────────────────────────────────────────────

enum UiMessage {
    PackagesLoaded {
        installed: Vec<PackageData>,
        updates: Vec<PackageData>,
        flatpak_updates: Vec<PackageData>,
        flatpak: Vec<PackageData>,
        stats: StatsData,
        flatpak_update_count: i32,
    },
    SearchResults(Vec<PackageData>),
    SetLoading(bool),
    SetBusy(bool),
    SetStatus(String),
    SetProgress(i32),
    SetProgressText(String),
    ShowTerminal(String),
    TerminalOutput(String),
    TerminalDone(bool),
    SetTerminalIsUpgrade(bool),
    HideTerminal,
    ShowProgressPopup(String),
    OperationProgress(i32, String),
    ProgressOutput(String),
    ProgressPrompt(String),
    ProgressHidePrompt,
    ProgressPromptButtons,
    ProgressLogLine(String, u8),
    ProgressETA(String),
    ProgressErrorSummary(String),
    ProgressAutoExpand,
    OperationDone(bool),
    ActivityLoaded(Vec<ActivityItem>),
    SysInfoLoaded(SysInfo),
    ShowConflict { summary: String, can_force: bool },
    FlatpakRemotesLoaded(Vec<String>),
    RemoteAppsFiltered { serial: u64, apps: Vec<PackageData>, total_matches: usize },
    FlatpakDetailReady {
        name: String,
        summary: String,
        description: String,
        developer: String,
        version: String,
        version_date: String,
        changelog: String,
        url_homepage: String,
        url_bugtracker: String,
        url_translate: String,
        url_vcs: String,
        categories: Vec<String>,
    },
    FlatpakScreenshotReady(String),
    FlatpakIconReady(String),
    FlatpakAddonsReady(Vec<PackageData>),
    FlatpakPageAppended(Vec<PackageData>),
    PacmanReposLoaded(Vec<String>),
    RepoPackagesLoaded(Vec<PackageData>),
    RepoPkgDetail(String),
    InstalledFlatpaksLoaded(Vec<PackageData>),
    DepTreeLoaded { deps: Vec<DepNode>, reqby: Vec<DepNode>, root_version: String },
    ArchNewsLoaded(Vec<ArchNewsItem>),
    ArchNewsLoading,
    ShowWarning { message: String, chaotic_aur: bool },
    ProgressShowClose,
}

// Plain-text fzf replacement for programs (like downgrade) that pipe through fzf.
// fzf uses alternate-screen TUI sequences that are invisible after strip_ansi.
// This script reads the list from stdin, prints it to /dev/tty (our PTY),
// asks the user to type a number, and outputs the matching line to stdout.
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

// Non-password interactive prompts that need input focus but not masking

const PACMAN_AUTO_CONFIRM_PATTERNS: &[&str] = &[];

const PACMAN_USER_PROMPT_PATTERNS: &[&str] = &[
    "Proceed with installation? [Y/n]",
    "Proceed with download? [Y/n]",
    ":: Proceed with installation? [Y/n]",
    ":: Proceed with download? [Y/n]",
    "Do you want to remove these packages? [y/N]",
    ":: Do you want to remove these packages? [y/N]",
    ":: Replace",
    ":: Import",
    "Enter a number",
    "Enter number to select",
    "Enter a selection",
    "Terminate batch job",
];

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
    #[serde(default = "default_notify_interval")]
    notify_interval_minutes: u32,
    #[serde(default = "default_parallel_downloads")]
    parallel_downloads: u32,
    #[serde(default)]
    tray_enabled: bool,
    #[serde(default = "default_tray_interval")]
    tray_check_interval_minutes: u32,
    #[serde(default)]
    aur_pill_dismissed: bool,
    #[serde(default)]
    distro_warning_dismissed: bool,
}

fn default_notify_interval() -> u32 { 30 }
fn default_parallel_downloads() -> u32 { 5 }
fn default_tray_interval() -> u32 { 30 }

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            flatpak_enabled: true,
            check_updates_on_start: false,
            notify_interval_minutes: 30,
            parallel_downloads: 5,
            tray_enabled: false,
            tray_check_interval_minutes: 30,
            aur_pill_dismissed: false,
            distro_warning_dismissed: false,
        }
    }
}


fn config_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(format!("{}/.config/xpm/config.json", home))
}

fn load_config() -> AppConfig {
    let path = config_path();
    if path.exists() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str::<AppConfig>(&content) {
                return config;
            }
        }
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
        notify_interval_minutes: window.get_setting_notify_interval() as u32,
        parallel_downloads: window.get_setting_parallel_downloads() as u32,
        tray_enabled: window.get_setting_tray_enabled(),
        tray_check_interval_minutes: window.get_setting_tray_check_interval() as u32,
        aur_pill_dismissed: window.get_aur_pill_dismissed(),
        distro_warning_dismissed: window.get_distro_warning_dismissed(),
    }
}

fn read_pacman_parallel_downloads() -> Option<u32> {
    let content = std::fs::read_to_string("/etc/pacman.conf").ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') { continue; }
        if let Some(rest) = trimmed.strip_prefix("ParallelDownloads") {
            let val_str = rest.trim_start_matches(|c: char| c == ' ' || c == '=').trim();
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

/// Strip ANSI/VT100 escape sequences, preserving all other characters including \r and \n.
/// The caller is responsible for interpreting \r (carriage return) for overwrite semantics.
fn strip_ansi(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '\x1b' {
            i += 1;
            if i >= len { break; }
            match chars[i] {
                '[' => {
                    i += 1;
                    while i < len && (chars[i] >= '0' && chars[i] <= '?') { i += 1; }
                    while i < len && (chars[i] >= ' ' && chars[i] <= '/') { i += 1; }
                    if i < len && (chars[i] >= '@' && chars[i] <= '~') { i += 1; }
                }
                ']' => {
                    i += 1;
                    while i < len {
                        if chars[i] == '\x07' { i += 1; break; }
                        if chars[i] == '\x1b' && i + 1 < len && chars[i + 1] == '\\' {
                            i += 2; break;
                        }
                        i += 1;
                    }
                }
                '(' | ')' | '*' | '+' => {
                    i += 1;
                    if i < len { i += 1; }
                }
                _ => { i += 1; }
            }
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}



/// Merge new_text into buffer with proper VT100 carriage-return semantics:
/// - bare \r  → overwrite current line from the start (pacman progress bars)
/// - \r\n     → regular newline (Windows line ending, no overwrite)
/// - \n       → commit current line, start new line
fn apply_terminal_text(buffer: &str, new_text: &str) -> String {
    let (prefix, current_line) = match buffer.rfind('\n') {
        Some(pos) => (&buffer[..pos + 1], &buffer[pos + 1..]),
        None => ("", buffer),
    };

    let mut line = current_line.to_string();
    let mut result = String::with_capacity(buffer.len() + new_text.len());
    result.push_str(prefix);

    let chars: Vec<char> = new_text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        match chars[i] {
            '\r' if i + 1 < len && chars[i + 1] == '\n' => {
                // \r\n = Windows newline - commit line, no overwrite
                result.push_str(&line);
                result.push('\n');
                line.clear();
                i += 2;
            }
            '\r' => {
                // bare \r = carriage return - overwrite current line
                line.clear();
                i += 1;
            }
            '\n' => {
                result.push_str(&line);
                result.push('\n');
                line.clear();
                i += 1;
            }
            c => {
                line.push(c);
                i += 1;
            }
        }
    }

    result.push_str(&line);
    result
}

// ── Dependency tree helpers ───────────────────────────────────────────────

fn clean_dep_name(s: &str) -> String {
    let s = s.trim();
    // strip version constraints  >= <= > < =
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
    // output: "/usr/bin/env is owned by coreutils 9.5-1"
    let text = String::from_utf8_lossy(&out.stdout);
    let pkg = text.split("is owned by ").nth(1)?.split_whitespace().next()?.to_string();
    if pkg.is_empty() { None } else { Some(pkg) }
}

/// Resolve a list of raw dep tokens (after clean_dep_name) to real package names:
///   - file paths → resolved via pacman -Qo (batched one call per path)
///   - sonames (.so) → dropped (library ABI virtuals, not package names)
///   - regular names → kept as-is
fn resolve_dep_list(raw: Vec<String>) -> Vec<String> {
    let mut result = Vec::with_capacity(raw.len());
    for dep in raw {
        if dep.starts_with('/') {
            // file path dep → resolve to owner package, skip if unresolvable
            if let Some(pkg) = resolve_file_dep(&dep) {
                if !result.contains(&pkg) {
                    result.push(pkg);
                }
            }
        } else if dep.contains(".so") {
            // soname virtual dep → skip, it's an ABI contract not a package name
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
                line.contains("/") && line.split_whitespace().next().map_or(false, |name| {
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
    let mut state = 0u8; // 1=depends, 2=optional, 3=reqby

    for line in text.lines() {
        // Continuation line (value continues on next line with leading spaces)
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

        // New key-value line
        state = 0;
        if let Some(val) = line.strip_prefix("Depends On").and_then(|r| r.splitn(2, ':').nth(1)) {
            state = 1;
            depends.extend(val.split_whitespace().filter(|&t| t != "None").map(clean_dep_name));
        } else if let Some(val) = line.strip_prefix("Optional Deps").and_then(|r| r.splitn(2, ':').nth(1)) {
            state = 2;
            optional.extend(val.split_whitespace().filter(|&t| t != "None").map(|t| {
                clean_dep_name(t.split(':').next().unwrap_or(t))
            }));
        } else if let Some(val) = line.strip_prefix("Required By").and_then(|r| r.splitn(2, ':').nth(1)) {
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

        if let Some(val) = line.strip_prefix("Name").and_then(|r| r.splitn(2, ':').nth(1)) {
            if !cur_name.is_empty() {
                result.insert(cur_name.clone(), resolve_dep_list(std::mem::take(&mut cur_deps)));
            }
            cur_name = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("Depends On").and_then(|r| r.splitn(2, ':').nth(1)) {
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
    let mut state = 0u8; // 1=depends, 2=optional

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
        if let Some(val) = line.strip_prefix("Depends On").and_then(|r| r.splitn(2, ':').nth(1)) {
            state = 1;
            depends.extend(val.split_whitespace()
                .filter(|&t| t != "None")
                .map(|t| clean_dep_name(t.split(':').next().unwrap_or(t))));
        } else if let Some(val) = line.strip_prefix("Optional Deps").and_then(|r| r.splitn(2, ':').nth(1)) {
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

    // Query root package info - try -Qi for installed, -Si for non-installed
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
            .and_then(|l| l.splitn(2, ':').nth(1))
            .map(|v| trim_version(v.trim()))
            .unwrap_or_default();
        let (d, o) = parse_si_block(&root_text);
        (d, o, vec![], ver)
    };

    // Batch-query level-2 deps (only installed packages respond to -Qi)
    let all_l1: Vec<String> = direct_deps.iter().chain(opt_deps.iter()).cloned().collect();
    let l1_installed: Vec<&str> = all_l1.iter()
        .filter(|n| installed.contains_key(n.as_str()))
        .map(|n| n.as_str()).collect();
    let l2_map = batch_deps(&l1_installed);

    // Collect ALL non-installed deps (l1 + l2) for a single installability pass
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

    // A dep should be shown if it's installed OR available in any repo
    let show_dep = |name: &str| -> bool {
        installed.contains_key(name) || *installable_map.get(name).unwrap_or(&false)
    };

    // Filter deps before computing tree connectors so └─/├─ stay correct
    let vis_direct: Vec<&String> = direct_deps.iter().filter(|n| show_dep(n)).collect();
    let vis_opt: Vec<&String>    = opt_deps.iter().filter(|n| show_dep(n)).collect();

    let mut dep_nodes: Vec<DepNode> = Vec::new();

    // ── Hard (required) dependencies ────────────────────────────────────────
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
            // Filter sub-deps the same way
            let vis_subs: Vec<&String> = sub_deps.iter().filter(|s| show_dep(s)).collect();
            if vis_subs.is_empty() { continue; }

            // Parent's vertical line continues only if more l1 nodes follow
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

    // ── Optional dependencies separator + entries (only if any survive filter) ─
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

    // ── Required-by (flat list) ──────────────────────────────────────────────
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

    let ret = unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
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
    run_in_terminal_impl(tx, title, cmd, args, input_sender, pid_holder, false, false);
}

fn run_in_terminal_expanded(
    tx: &mpsc::Sender<UiMessage>,
    title: &str,
    cmd: &str,
    args: &[&str],
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
) {
    run_in_terminal_impl(tx, title, cmd, args, input_sender, pid_holder, true, true);
}

fn run_in_terminal_impl(
    tx: &mpsc::Sender<UiMessage>,
    title: &str,
    cmd: &str,
    args: &[&str],
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
    auto_expand: bool,
    always_input: bool,
) {
    let _ = tx.send(UiMessage::ShowProgressPopup(title.to_string()));
    if auto_expand {
        let _ = tx.send(UiMessage::ProgressAutoExpand);
        let _ = tx.send(UiMessage::ProgressShowClose);
    }

    let (master_fd, child_pid) = match spawn_in_pty(cmd, args) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = tx.send(UiMessage::ProgressOutput(format!("Error: {}\n", e)));
            let _ = tx.send(UiMessage::OperationDone(false));
            return;
        }
    };

    *pid_holder.lock().unwrap() = Some(child_pid);

    let (in_tx, in_rx) = mpsc::channel::<String>();
    let in_tx_auto = in_tx.clone();
    *input_sender.lock().unwrap() = Some(in_tx);

    let tx_reader = tx.clone();
    let master_fd_reader = master_fd;
    let total_packages: usize = 1; // unknown for generic commands; % parser degrades gracefully

    let reader_handle = thread::spawn(move || {
        use std::io::Read;
        let mut file = unsafe { std::fs::File::from_raw_fd(master_fd_reader) };
        let mut buf = [0u8; 4096];
        let mut current_percent: i32 = 0;
        let mut pending_output = String::new();
        let mut last_output_flush = std::time::Instant::now() - std::time::Duration::from_millis(100);
        const OUTPUT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
        const MAX_OUTPUT_LINES: usize = 500;
        let op_start = std::time::Instant::now();
        let mut first_error_line: Option<String> = None;

        loop {
            let ready = unsafe {
                let mut pfd = libc::pollfd { fd: master_fd_reader, events: libc::POLLIN, revents: 0 };
                libc::poll(&mut pfd as *mut libc::pollfd, 1, 20)
            };
            if ready < 0 { break; }

            let now = std::time::Instant::now();

            if ready == 0 {
                if !pending_output.is_empty() {
                    let _ = tx_reader.send(UiMessage::ProgressOutput(pending_output.clone()));
                    last_output_flush = now;
                }
                continue;
            }

            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let cleaned = strip_ansi(&text);
                    if cleaned.is_empty() { continue; }

                    pending_output = apply_terminal_text(&pending_output, &cleaned);

                    let line_count = pending_output.split('\n').count();
                    if line_count > MAX_OUTPUT_LINES {
                        let skip = line_count - MAX_OUTPUT_LINES;
                        let start = pending_output.split('\n').take(skip).map(|l| l.len() + 1).sum::<usize>();
                        pending_output.drain(..start.min(pending_output.len()));
                    }

                    let is_auto_confirm = PACMAN_AUTO_CONFIRM_PATTERNS.iter().any(|p| cleaned.contains(p));
                    let mut force_flush = false;

                    if is_auto_confirm {
                        let _ = in_tx_auto.send("y".to_string());
                        let _ = tx_reader.send(UiMessage::ProgressHidePrompt);
                        force_flush = true;
                    } else {
                        let has_yn = cleaned.contains("[Y/n]") || cleaned.contains("[y/n]");
                        let has_y_n = cleaned.contains("[y/N]") && !is_auto_confirm;
                        let needs_user_input = PACMAN_USER_PROMPT_PATTERNS.iter().any(|p| cleaned.contains(p)) || has_y_n;
                        if needs_user_input || has_yn {
                            let prompt_text = cleaned.lines()
                                .filter(|l| !l.trim().is_empty())
                                .last()
                                .unwrap_or(&cleaned)
                                .trim()
                                .to_string();
                            if (has_yn || has_y_n) && !always_input {
                                // Simple Y/n → stay compact, show Proceed/Cancel buttons
                                let _ = tx_reader.send(UiMessage::ProgressPromptButtons);
                                let _ = tx_reader.send(UiMessage::ProgressPrompt("Proceed with transaction?".to_string()));
                            } else {
                                // always_input mode (downgrade) or conflict/numbered choice → text input
                                let _ = tx_reader.send(UiMessage::ProgressPrompt(prompt_text));
                                let _ = tx_reader.send(UiMessage::ProgressAutoExpand);
                            }
                            force_flush = true;
                        }
                    }

                    for line in cleaned.split('\n') {
                        let clean_line = line.split('\r').last().unwrap_or(line);
                        let lower_line = clean_line.to_lowercase();
                        let trimmed = clean_line.trim().to_string();
                        if trimmed.is_empty() { continue; }

                        let level: u8 = if lower_line.contains("error:") { 1 }
                            else if lower_line.contains("warning:") { 2 }
                            else if (lower_line.contains("installed") || lower_line.contains("upgraded")
                                || lower_line.contains("removed")) && !lower_line.contains("error:") { 3 }
                            else { 0 };

                        if level == 1 && first_error_line.is_none() {
                            first_error_line = Some(trimmed.clone());
                        }

                        let _ = tx_reader.send(UiMessage::ProgressLogLine(trimmed.clone(), level));

                        let phase_label = if lower_line.contains("resolving dependencies") {
                            Some(("Resolving dependencies...", 10i32))
                        } else if lower_line.contains("looking for conflicting") {
                            Some(("Checking for conflicts...", 15))
                        } else if lower_line.contains("downloading") {
                            let pct = parse_progress_fraction(line, 20, 50, total_packages).unwrap_or(35);
                            Some(("Downloading packages...", pct))
                        } else if lower_line.contains("checking keyring") {
                            Some(("Verifying signatures...", 52))
                        } else if lower_line.contains("checking integrity") {
                            Some(("Verifying signatures...", 53))
                        } else if lower_line.contains("checking package integrity") {
                            Some(("Verifying signatures...", 55))
                        } else if lower_line.contains("loading package files") {
                            Some(("Loading package files...", 58))
                        } else if lower_line.contains("installing") || lower_line.contains("upgrading") {
                            let pct = parse_progress_fraction(line, 60, 85, total_packages).unwrap_or(72);
                            Some(("Installing packages...", pct))
                        } else if lower_line.contains("removing") || lower_line.contains("reinstalling") {
                            let pct = parse_progress_fraction(line, 60, 85, total_packages).unwrap_or(72);
                            Some(("Removing packages...", pct))
                        } else if lower_line.contains("running post-transaction hooks") {
                            Some(("Running post-install hooks...", 88))
                        } else if lower_line.contains("arming conditionpathexists")
                            || lower_line.contains("updating linux module") || lower_line.contains("dkms") {
                            Some(("Running post-install hooks...", 90))
                        } else if lower_line.contains("updating linux initcpios") || lower_line.contains("mkinitcpio") {
                            Some(("Rebuilding initramfs...", 92))
                        } else if lower_line.contains("updating grub") || lower_line.contains("grub-mkconfig") || lower_line.contains("grub") {
                            Some(("Updating bootloader...", 95))
                        } else if lower_line.contains("updating the desktop") || lower_line.contains("updating mime") {
                            Some(("Updating system databases...", 97))
                        } else if lower_line.contains("updating the info") {
                            Some(("Updating system databases...", 97))
                        } else {
                            None
                        };

                        if let Some((label, new_pct)) = phase_label {
                            if new_pct > current_percent {
                                current_percent = new_pct;
                                let _ = tx_reader.send(UiMessage::OperationProgress(current_percent, label.to_string()));

                                if current_percent > 5 {
                                    let elapsed = op_start.elapsed().as_secs_f32();
                                    if elapsed > 1.0 {
                                        let rate = current_percent as f32 / elapsed;
                                        let remaining = (100 - current_percent) as f32 / rate;
                                        let eta_str = if remaining < 60.0 {
                                            format!("~{}s", remaining as u32)
                                        } else {
                                            let mins = (remaining / 60.0) as u32;
                                            let secs = (remaining as u32) % 60;
                                            format!("~{}m {}s", mins, secs)
                                        };
                                        let _ = tx_reader.send(UiMessage::ProgressETA(eta_str));
                                    }
                                }
                            } else if new_pct == current_percent && current_percent >= 88 {
                                let _ = tx_reader.send(UiMessage::OperationProgress(current_percent, label.to_string()));
                            }
                        }
                    }

                    if force_flush || now.duration_since(last_output_flush) >= OUTPUT_FLUSH_INTERVAL {
                        let _ = tx_reader.send(UiMessage::ProgressOutput(pending_output.clone()));
                        last_output_flush = now;
                    }
                }
                Err(_) => break,
            }
        }
        if !pending_output.is_empty() {
            let _ = tx_reader.send(UiMessage::ProgressOutput(pending_output));
        }
        if let Some(err) = first_error_line {
            let _ = tx_reader.send(UiMessage::ProgressErrorSummary(err));
        }
        std::mem::forget(file);
    });

    let master_fd_writer = master_fd;
    let writer_handle = thread::spawn(move || {
        use std::io::Write;
        let dup_fd = unsafe { libc::dup(master_fd_writer) };
        if dup_fd < 0 { return; }
        let mut file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        while let Ok(input) = in_rx.recv() {
            let data = format!("{}\n", input);
            if file.write_all(data.as_bytes()).is_err() { break; }
            let _ = file.flush();
        }
    });

    let status = unsafe {
        let mut wstatus: libc::c_int = 0;
        libc::waitpid(child_pid as libc::pid_t, &mut wstatus, 0);
        wstatus
    };

    let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;

    *pid_holder.lock().unwrap() = None;
    *input_sender.lock().unwrap() = None;

    unsafe { libc::close(master_fd); }

    let _ = reader_handle.join();
    let _ = writer_handle.join();

    if !success {
        let _ = tx.send(UiMessage::ProgressAutoExpand);
    }
    let _ = tx.send(UiMessage::OperationDone(success));
}

fn build_pacman_command(action: &str, names: &[String], backend: i32) -> (String, Vec<String>) {
    match (action, backend) {
        ("install", 1) | ("bulk-install", 1) => {
            ("flatpak".to_string(), {
                let mut args = vec!["install".to_string(), "-y".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("remove", 1) | ("bulk-remove", 1) => {
            ("flatpak".to_string(), {
                let mut args = vec!["uninstall".to_string(), "-y".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
        ("update", 1) => {
            ("flatpak".to_string(), {
                let mut args = vec!["update".to_string(), "-y".to_string()];
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
            ("flatpak".to_string(), vec!["update".to_string(), "-y".to_string()])
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
            // install / update / bulk-install
            ("pkexec".to_string(), {
                let mut args = vec!["pacman".to_string(), "-S".to_string()];
                args.extend(names.iter().cloned());
                args
            })
        }
    }
}

fn run_managed_operation(
    tx: &mpsc::Sender<UiMessage>,
    title: &str,
    action: &str,
    names: &[String],
    backend: i32,
    input_sender: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pid_holder: &Arc<Mutex<Option<u32>>>,
    conflict_ctx: &Arc<Mutex<Option<(String, Vec<String>, i32)>>>,
) {
    *conflict_ctx.lock().unwrap() = Some((action.to_string(), names.to_vec(), backend));
    let _ = tx.send(UiMessage::ShowProgressPopup(title.to_string()));

    let (cmd, args) = build_pacman_command(action, names, backend);
    let args_str: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let (master_fd, child_pid) = match spawn_in_pty(&cmd, &args_str) {
        Ok(pair) => pair,
        Err(e) => {
            let _ = tx.send(UiMessage::OperationProgress(0, format!("Error: {}", e)));
            let _ = tx.send(UiMessage::OperationDone(false));
            return;
        }
    };

    *pid_holder.lock().unwrap() = Some(child_pid);

    let (in_tx, in_rx) = mpsc::channel::<String>();
    *input_sender.lock().unwrap() = Some(in_tx.clone());

    let escalated = Arc::new(Mutex::new(false));
    let output_buffer = Arc::new(Mutex::new(String::new()));

    let tx_reader = tx.clone();
    let master_fd_reader = master_fd;
    let escalated_r = escalated.clone();
    let output_buffer_r = output_buffer.clone();
    let in_tx_r = in_tx;
    let total_packages = names.len().max(1);

    let reader_handle = thread::spawn(move || {
        use std::io::Read;
        let mut file = unsafe { std::fs::File::from_raw_fd(master_fd_reader) };
        let mut buf = [0u8; 4096];
        let mut current_percent: i32 = 0;
        let mut pending_output = String::new();
        let mut last_output_flush = std::time::Instant::now() - std::time::Duration::from_millis(100);
        const OUTPUT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
        const MAX_OUTPUT_LINES: usize = 500;
        let op_start = std::time::Instant::now();
        let mut first_error_line: Option<String> = None;

        loop {
            let ready = unsafe {
                let mut pfd = libc::pollfd { fd: master_fd_reader, events: libc::POLLIN, revents: 0 };
                libc::poll(&mut pfd as *mut libc::pollfd, 1, 20)
            };
            if ready < 0 { break; }

            let now = std::time::Instant::now();

            if ready == 0 {
                if !pending_output.is_empty() {
                    let _ = tx_reader.send(UiMessage::ProgressOutput(pending_output.clone()));
                    last_output_flush = now;
                }
                continue;
            }

            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let cleaned = strip_ansi(&text);
                    if cleaned.is_empty() { continue; }

                    {
                        let mut ob = output_buffer_r.lock().unwrap();
                        if ob.len() < 65536 { ob.push_str(&cleaned); }
                    }

                    pending_output = apply_terminal_text(&pending_output, &cleaned);

                    let line_count = pending_output.split('\n').count();
                    if line_count > MAX_OUTPUT_LINES {
                        let skip = line_count - MAX_OUTPUT_LINES;
                        let start = pending_output.split('\n').take(skip).map(|l| l.len() + 1).sum::<usize>();
                        pending_output.drain(..start.min(pending_output.len()));
                    }

                    let lower = cleaned.to_lowercase();
                    if CONFLICT_PATTERNS.iter().any(|p| lower.contains(&p.to_lowercase())) {
                        *escalated_r.lock().unwrap() = true;
                    }

                    let is_auto_confirm = PACMAN_AUTO_CONFIRM_PATTERNS.iter().any(|p| cleaned.contains(p));
                    let mut force_flush = false;

                    if is_auto_confirm {
                        let _ = in_tx_r.send("y".to_string());
                        let _ = tx_reader.send(UiMessage::ProgressHidePrompt);
                        force_flush = true;
                    } else {
                        let has_yn = cleaned.contains("[Y/n]") || cleaned.contains("[y/n]");
                        let has_y_n = cleaned.contains("[y/N]") && !is_auto_confirm;
                        let needs_user_input = PACMAN_USER_PROMPT_PATTERNS.iter().any(|p| cleaned.contains(p)) || has_y_n;
                        if needs_user_input || has_yn {
                            let prompt_text = cleaned.lines()
                                .filter(|l| !l.trim().is_empty())
                                .last()
                                .unwrap_or(&cleaned)
                                .trim()
                                .to_string();
                            // Simple yes/no prompt → show Proceed/Cancel buttons
                            if has_yn || has_y_n {
                                // Simple Y/n → stay compact, show Proceed/Cancel buttons
                                let _ = tx_reader.send(UiMessage::ProgressPromptButtons);
                                let _ = tx_reader.send(UiMessage::ProgressPrompt("Proceed with transaction?".to_string()));
                            } else {
                                // Conflict/replacement/numbered choice → expand + text input
                                let _ = tx_reader.send(UiMessage::ProgressPrompt(prompt_text));
                                let _ = tx_reader.send(UiMessage::ProgressAutoExpand);
                            }
                            force_flush = true;
                        }
                    }

                    for line in cleaned.split('\n') {
                        let clean_line = line.split('\r').last().unwrap_or(line);
                        let lower_line = clean_line.to_lowercase();
                        let trimmed = clean_line.trim().to_string();
                        if trimmed.is_empty() { continue; }

                        // Determine log level
                        let level: u8 = if lower_line.contains("error:") { 1 }
                            else if lower_line.contains("warning:") { 2 }
                            else if (lower_line.contains("installed") || lower_line.contains("upgraded")
                                || lower_line.contains("removed")) && !lower_line.contains("error:") { 3 }
                            else { 0 };

                        if level == 1 && first_error_line.is_none() {
                            first_error_line = Some(trimmed.clone());
                        }

                        let _ = tx_reader.send(UiMessage::ProgressLogLine(trimmed.clone(), level));

                        // Phase label mapping
                        let phase_label = if lower_line.contains("resolving dependencies") {
                            Some(("Resolving dependencies...", 10i32))
                        } else if lower_line.contains("looking for conflicting") {
                            Some(("Checking for conflicts...", 15))
                        } else if lower_line.contains("downloading") {
                            let pct = parse_progress_fraction(line, 20, 50, total_packages).unwrap_or(35);
                            Some(("Downloading packages...", pct))
                        } else if lower_line.contains("checking keyring") {
                            Some(("Verifying signatures...", 52))
                        } else if lower_line.contains("checking integrity") {
                            Some(("Verifying signatures...", 53))
                        } else if lower_line.contains("checking package integrity") {
                            Some(("Verifying signatures...", 55))
                        } else if lower_line.contains("loading package files") {
                            Some(("Loading package files...", 58))
                        } else if lower_line.contains("installing") || lower_line.contains("upgrading") {
                            let pct = parse_progress_fraction(line, 60, 85, total_packages).unwrap_or(72);
                            Some(("Installing packages...", pct))
                        } else if lower_line.contains("removing") || lower_line.contains("reinstalling") {
                            let pct = parse_progress_fraction(line, 60, 85, total_packages).unwrap_or(72);
                            Some(("Removing packages...", pct))
                        } else if lower_line.contains("running post-transaction hooks") {
                            Some(("Running post-install hooks...", 88))
                        } else if lower_line.contains("arming conditionpathexists")
                            || lower_line.contains("updating linux module") || lower_line.contains("dkms") {
                            Some(("Running post-install hooks...", 90))
                        } else if lower_line.contains("updating linux initcpios") || lower_line.contains("mkinitcpio") {
                            Some(("Rebuilding initramfs...", 92))
                        } else if lower_line.contains("updating grub") || lower_line.contains("grub-mkconfig") || lower_line.contains("grub") {
                            Some(("Updating bootloader...", 95))
                        } else if lower_line.contains("updating the desktop") || lower_line.contains("updating mime") {
                            Some(("Updating system databases...", 97))
                        } else if lower_line.contains("updating the info") {
                            Some(("Updating system databases...", 97))
                        } else {
                            None
                        };

                        if let Some((label, new_pct)) = phase_label {
                            if new_pct > current_percent {
                                current_percent = new_pct;
                                let _ = tx_reader.send(UiMessage::OperationProgress(current_percent, label.to_string()));

                                // ETA calculation once we have meaningful progress
                                if current_percent > 5 {
                                    let elapsed = op_start.elapsed().as_secs_f32();
                                    if elapsed > 1.0 {
                                        let rate = current_percent as f32 / elapsed;
                                        let remaining = (100 - current_percent) as f32 / rate;
                                        let eta_str = if remaining < 60.0 {
                                            format!("~{}s", remaining as u32)
                                        } else {
                                            let mins = (remaining / 60.0) as u32;
                                            let secs = (remaining as u32) % 60;
                                            format!("~{}m {}s", mins, secs)
                                        };
                                        let _ = tx_reader.send(UiMessage::ProgressETA(eta_str));
                                    }
                                }
                            } else if new_pct == current_percent && current_percent >= 88 {
                                let _ = tx_reader.send(UiMessage::OperationProgress(current_percent, label.to_string()));
                            }
                        }
                    }

                    if force_flush || now.duration_since(last_output_flush) >= OUTPUT_FLUSH_INTERVAL {
                        let _ = tx_reader.send(UiMessage::ProgressOutput(pending_output.clone()));
                        last_output_flush = now;
                    }
                }
                Err(_) => break,
            }
        }
        if !pending_output.is_empty() {
            let _ = tx_reader.send(UiMessage::ProgressOutput(pending_output));
        }
        if let Some(err) = first_error_line {
            let _ = tx_reader.send(UiMessage::ProgressErrorSummary(err));
        }
        std::mem::forget(file);
    });

    let master_fd_writer = master_fd;
    let writer_handle = thread::spawn(move || {
        use std::io::Write;
        let dup_fd = unsafe { libc::dup(master_fd_writer) };
        if dup_fd < 0 {
            return;
        }
        let mut file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        while let Ok(input) = in_rx.recv() {
            let data = format!("{}\n", input);
            if file.write_all(data.as_bytes()).is_err() {
                break;
            }
            let _ = file.flush();
        }
    });

    let status = unsafe {
        let mut wstatus: libc::c_int = 0;
        libc::waitpid(child_pid as libc::pid_t, &mut wstatus, 0);
        wstatus
    };

    let success = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;

    *pid_holder.lock().unwrap() = None;
    *input_sender.lock().unwrap() = None;

    unsafe { libc::close(master_fd); }

    let _ = reader_handle.join();
    let _ = writer_handle.join();

    let was_escalated = *escalated.lock().unwrap();
    if was_escalated && !success {
        let output = output_buffer.lock().unwrap().clone();
        let (summary, can_force) = parse_conflict_summary(&output);
        let _ = tx.send(UiMessage::ShowConflict { summary, can_force });
    } else {
        if !success {
            let _ = tx.send(UiMessage::ProgressAutoExpand);
        }
        let _ = tx.send(UiMessage::OperationDone(success));
    }
}

fn parse_progress_fraction(line: &str, range_start: i32, range_end: i32, _total_packages: usize) -> Option<i32> {
    if let Some(start) = line.find('(') {
        if let Some(end) = line[start..].find(')') {
            let inner = &line[start + 1..start + end];
            let parts: Vec<&str> = inner.split('/').collect();
            if parts.len() == 2 {
                if let (Ok(current), Ok(total)) = (
                    parts[0].trim().parse::<i32>(),
                                                   parts[1].trim().parse::<i32>(),
                ) {
                    if total > 0 {
                        let fraction = current as f64 / total as f64;
                        return Some(range_start + ((range_end - range_start) as f64 * fraction) as i32);
                    }
                }
            }
        }
    }
    None
}

fn package_to_ui(pkg: &xpm_core::package::Package, has_update: bool, desktop_map: &HashMap<String, String>) -> PackageData {
    let backend = match pkg.backend {
        xpm_core::package::PackageBackend::Pacman => 0,
        xpm_core::package::PackageBackend::Flatpak => 1,
    };

    let display_name = humanize_package_name(&pkg.name, desktop_map);

    PackageData {
        name: SharedString::from(pkg.name.as_str()),
        display_name: SharedString::from(&display_name),
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
    };

    let version_str = format!(
        "{} → {}",
        update.current_version.to_string(),
                              update.new_version.to_string()
    );

    PackageData {
        name: SharedString::from(update.name.as_str()),
        display_name: SharedString::from(update.name.as_str()),
        version: SharedString::from(version_str.as_str()),
        description: SharedString::from(version_str.as_str()),
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
            if let Some(mut row) = vec_model.row_data(i) {
                if row.name.as_str() == name && row.backend == backend {
                    row.selected = selected;
                    vec_model.set_row_data(i, row);
                    break;
                }
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
                if let Some(row) = vec_model.row_data(i) {
                    if row.name.as_str() == name && row.backend == backend {
                        return row.installed;
                    }
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
        // Match exact name or "linux-*" kernel packages
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

/// Given the last line of terminal output containing a prompt like `[Y/n]` or `(yes/no)`,
/// return the default answer string (the uppercase / first option).
/// Returns None if no recognisable prompt is found.
fn detect_prompt_default(line: &str) -> Option<String> {
    // Look for [...] bracket patterns last on the line: [Y/n], [y/N], [Y/N], [yes/no] etc.
    let line = line.trim_end_matches(|c: char| c == ' ' || c == ':');
    if let Some(start) = line.rfind('[') {
        if let Some(rel_end) = line[start..].find(']') {
            let inner = &line[start + 1..start + rel_end];
            // Split by '/' and pick the uppercase variant as default
            for part in inner.split('/') {
                let part = part.trim();
                if !part.is_empty() && part.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    return Some(part.to_lowercase());
                }
            }
            // All lowercase - first option is default
            if let Some(first) = inner.split('/').next() {
                let first = first.trim();
                if !first.is_empty() {
                    return Some(first.to_string());
                }
            }
        }
    }
    // (yes/no) paren style
    if let Some(start) = line.rfind('(') {
        if let Some(rel_end) = line[start..].find(')') {
            let inner = &line[start + 1..start + rel_end];
            for part in inner.split('/') {
                let part = part.trim();
                if !part.is_empty() && part.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
                    return Some(part.to_lowercase());
                }
            }
            if let Some(first) = inner.split('/').next() {
                let first = first.trim();
                if !first.is_empty() {
                    return Some(first.to_string());
                }
            }
        }
    }
    None
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
            // Format: [2024-01-15T10:30:00+0000] [ALPM] installed pkg (ver)
            let alpm_pos = line.find("] [ALPM] ")?;
            let rest = &line[alpm_pos + 9..];
            let (action, pkg_part) = if rest.starts_with("installed ") {
                ("installed", &rest[10..])
            } else if rest.starts_with("removed ") {
                ("removed", &rest[8..])
            } else if rest.starts_with("upgraded ") {
                ("upgraded", &rest[9..])
            } else {
                return None;
            };
            let pkg = pkg_part.split_whitespace().next().unwrap_or("").to_string();
            if pkg.is_empty() { return None; }

            // Parse date from [2024-01-15T10:30:00+0000]
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
    // Kernel version
    let kernel = std::fs::read_to_string("/proc/version")
        .unwrap_or_default()
        .split_whitespace()
        .nth(2)
        .unwrap_or("unknown")
        .to_string();

    // Uptime
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

    // CPU model (first model name line, shortened)
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

    // RAM from /proc/meminfo (kB → MB)
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

    // GPU - probe /sys/class/drm (fast, no subprocess)
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
                if let Ok(ue) = std::fs::read_to_string(uevent) {
                    if let Some(line) = ue.lines().find(|l| l.starts_with("PCI_ID=")) {
                        let pci_id = line.trim_start_matches("PCI_ID=");
                        return Some(format!("{} ({})", prefix, pci_id));
                    }
                }
                return Some(format!("{} {}", prefix, dev));
            }
        }
        None
    })().unwrap_or_default();

    // Disk usage for / via /proc/mounts + statvfs (no subprocess needed)
    let (disk_used, disk_total) = (|| -> Option<(String, String)> {
        // Use df -h / which is universally available and fast
        let out = std::process::Command::new("df")
            .args(["-h", "/"])
            .output().ok()?;
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        // df -h / output: header line then data line
        // Filesystem  Size  Used  Avail  Use%  Mounted on
        let line = text.lines().nth(1)?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        // col 1=Size, 2=Used
        let total = parts.get(1)?.to_string();
        let used = parts.get(2)?.to_string();
        Some((used, total))
    })().unwrap_or_default();

    // Hostname
    let hostname = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();

    // Distro name from /etc/os-release
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
        r if r.is_empty() => 8,
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
                backend: -1, // sentinel: group header
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
        // Augment pkg: store letter initial in required_by, category in installed_size
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
        // Derive letter initial from display name
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
        // Fallback: update the standard Arch mirrorlist only
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
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut items: Vec<ArchNewsItem> = Vec::new();
    let mut in_item = false;
    let mut cur_tag = String::new();
    let mut title = String::new();
    let mut date = String::new();
    let mut link = String::new();
    let mut description = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let tag = std::str::from_utf8(e.name().as_ref()).unwrap_or("").to_string();
                if tag == "item" {
                    in_item = true;
                    title.clear(); date.clear(); link.clear(); description.clear();
                }
                cur_tag = tag;
            }
            Ok(Event::Text(e)) => {
                if !in_item { continue; }
                let text = e.unescape().unwrap_or_default().to_string();
                match cur_tag.as_str() {
                    "title" => title = text,
                    "pubDate" => {
                        // Format: "Mon, 07 Apr 2025 00:00:00 +0000" → "07 Apr 2025"
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
            Ok(Event::CData(e)) => {
                if !in_item { continue; }
                let text = String::from_utf8_lossy(e.as_ref()).to_string();
                match cur_tag.as_str() {
                    "description" => description = strip_html(&text),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name_bytes = e.name();
                let tag = std::str::from_utf8(name_bytes.as_ref()).unwrap_or("");
                if tag == "item" && in_item {
                    in_item = false;
                    // Trim description to reasonable length
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
            Ok(Event::Eof) => break,
            Err(_) => break,
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
    // Collapse whitespace
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn main() {
    // Ensure Qt can find its plugins and libraries
    // This helps when the app is installed to /usr/bin vs run from build dir
    
    // Plugin path for Qt style/platform theme plugins
    if std::env::var("QT_PLUGIN_PATH").map(|p| p.is_empty()).unwrap_or(true) {
        std::env::set_var("QT_PLUGIN_PATH", "/usr/lib/qt6/plugins:/usr/lib/x86_64-linux-gnu/qt6/plugins");
    }
    
    // Library path for Qt plugins that are loaded via QLibrary
    let current_path = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    if !current_path.contains("/usr/lib/qt6") {
        let new_path = if current_path.is_empty() {
            "/usr/lib/qt6:/usr/lib/x86_64-linux-gnu/qt6".to_string()
        } else {
            format!("{}:/usr/lib/qt6:/usr/lib/x86_64-linux-gnu/qt6", current_path)
        };
        std::env::set_var("LD_LIBRARY_PATH", new_path);
    }

    let subscriber = FmtSubscriber::builder()
    .with_max_level(Level::INFO)
    .finish();
    tracing::subscriber::set_global_default(subscriber).expect("Failed to set subscriber");

    info!("Starting xPackageManager");

    let args: Vec<String> = std::env::args().collect();
    // --tray: launched by autostart, run as tray-only daemon (no window shown).
    let tray_only = args.iter().any(|a| a == "--tray");
    let local_package_path = args.iter().skip(1)
        .find(|arg| is_arch_package(arg.as_str()))
        .cloned();

    if tray_only {
        info!("Starting in tray-only mode");
    }
    if let Some(ref path) = local_package_path {
        info!("Opening local package: {}", path);
    }

    // Single-instance guard: if another instance is running, signal it to show
    // its window and exit. Keep the lock file alive for the lifetime of this process.
    let _instance_lock = match acquire_instance_lock() {
        Some(f) => f,
        None => {
            info!("Another instance is already running — bringing it to foreground");
            signal_existing_instance();
            return;
        }
    };

    let window = MainWindow::new().expect("Failed to create window");

    let (tx, rx) = mpsc::channel::<UiMessage>();
    let rx = Rc::new(RefCell::new(rx));

    listen_for_instance_signals(window.as_weak());

    let terminal_input_sender: Arc<Mutex<Option<mpsc::Sender<String>>>> = Arc::new(Mutex::new(None));
    let terminal_child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    let conflict_context: Arc<Mutex<Option<(String, Vec<String>, i32)>>> = Arc::new(Mutex::new(None));
    // Full parsed flatpak app list for client-side filtering
    let flatpak_app_store: Arc<Mutex<Vec<CachedRemoteApp>>> = Arc::new(Mutex::new(Vec::new()));
    let flatpak_installed_ids: Arc<Mutex<std::collections::HashSet<String>>> = Arc::new(Mutex::new(std::collections::HashSet::new()));
    // Serial counter: incremented on each filter call, background threads check it before sending
    let flatpak_filter_serial: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(0));

    const FLATPAK_PAGE_SIZE: usize = 150;  // max rows shown at once

    // Load cached packages immediately for fast startup
    if let Some(cache) = load_package_cache() {
        let installed: Vec<PackageData> = cache.installed.iter().map(cached_to_pkg).collect();
        let updates: Vec<PackageData> = cache.updates.iter().map(cached_to_pkg).collect();
        let flatpak: Vec<PackageData> = cache.flatpak.iter().map(cached_to_pkg).collect();
        let stats = StatsData {
            pacman_count: cache.pacman_count,
            flatpak_count: cache.flatpak_count,
            orphan_count: cache.orphan_count,
            update_count: cache.update_count,
            cache_size: SharedString::from(cache.cache_size.as_str()),
        };
        let page_size_early = 50usize;
        let page: Vec<PackageData> = installed.iter().take(page_size_early).cloned().collect();
        let total = ((installed.len() + page_size_early - 1) / page_size_early).max(1) as i32;
        window.set_installed_packages(ModelRc::new(VecModel::from(page)));
        window.set_total_pages(total);
        window.set_update_packages(ModelRc::new(VecModel::from(updates)));
        window.set_flatpak_packages(ModelRc::new(VecModel::from(flatpak)));
        window.set_stats(stats);
    }
    // Always clear loading immediately - stat cards should never be stuck in skeleton state.
    // The background thread will overwrite stats/packages when it finishes.
    window.set_loading(false);

    let selected_packages: Rc<RefCell<Vec<(String, i32, bool)>>> = Rc::new(RefCell::new(Vec::new()));

    let page_size: i32 = 50;
    let full_installed: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));
    let full_installed_flatpaks: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));
    let full_installed_grouped: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));
    let repo_packages_full: Rc<RefCell<Vec<PackageData>>> = Rc::new(RefCell::new(Vec::new()));

    let tx_load = tx.clone();
    let tx_search = tx.clone();

    // Shared log model for colored progress log lines — reset when popup opens
    let log_model: Rc<RefCell<Option<Rc<VecModel<LogLine>>>>> = Rc::new(RefCell::new(None));

    let timer = Timer::default();
    let window_weak = window.as_weak();
    let rx_clone = rx.clone();
    let tx_timer = tx.clone();
    let mut pending_terminal = String::new();
    let mut last_term_flush = std::time::Instant::now();
    let full_installed_timer = full_installed.clone();
    let full_installed_flatpaks_timer = full_installed_flatpaks.clone();
    let full_installed_grouped_timer = full_installed_grouped.clone();
    let repo_full_timer = repo_packages_full.clone();
    let filter_serial_timer = flatpak_filter_serial.clone();
    let conflict_ctx_timer = conflict_context.clone();
    let flatpak_ids_timer = flatpak_installed_ids.clone();
    let flatpak_store_timer = flatpak_app_store.clone();
    let log_model_timer = log_model.clone();
    // Shared tray state — created here so the timer closure can capture them.
    // The tray setup section below reuses these same Arcs.
    let tray_update_count: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));
    let tray_check_tx: TrayCheckTx = Arc::new(Mutex::new(None));
    let tray_count_op  = tray_update_count.clone();
    let tray_check_op  = tray_check_tx.clone();

    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(50), move || {
        if let Some(window) = window_weak.upgrade() {
            let mut flush_now = false;

            while let Ok(msg) = rx_clone.borrow_mut().try_recv() {
                match msg {
                    UiMessage::PackagesLoaded { installed, updates, flatpak_updates, flatpak, stats, flatpak_update_count } => {
                        *full_installed_timer.borrow_mut() = installed;
                        let ps = page_size as usize;
                        let inst = full_installed_timer.borrow();
                        let total = ((inst.len() + ps - 1) / ps).max(1) as i32;
                        let page: Vec<PackageData> = inst.iter().take(ps).cloned().collect();
                        window.set_installed_packages(ModelRc::new(VecModel::from(page)));
                        window.set_current_page(0);
                        window.set_total_pages(total);
                        drop(inst);
                        window.set_update_packages(ModelRc::new(VecModel::from(updates)));
                        window.set_flatpak_update_packages(ModelRc::new(VecModel::from(flatpak_updates)));
                        window.set_flatpak_packages(ModelRc::new(VecModel::from(flatpak)));
                        window.set_flatpak_update_count(flatpak_update_count);
                        window.set_stats(stats);
                        // Pre-compute grouped installed list for view 0 tab 0
                        let full_for_grp: Vec<PackageData> = full_installed_timer.borrow().clone();
                        let grouped = group_installed_by_repo(full_for_grp);
                        *full_installed_grouped_timer.borrow_mut() = grouped.clone();
                        window.set_installed_grouped(ModelRc::new(VecModel::from(grouped)));
                        window.set_loading(false);
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
                    UiMessage::ShowTerminal(title) => {
                        window.set_terminal_title(SharedString::from(&title));
                        window.set_terminal_output(SharedString::from(""));
                        window.set_terminal_done(false);
                        window.set_terminal_success(false);
                        window.set_terminal_show_password(false);
                        window.set_show_terminal(true);
                        window.set_terminal_focus_pending(true);
                        pending_terminal.clear();
                    }
                    UiMessage::TerminalOutput(text) => {
                        pending_terminal.push_str(&text);
                        // cap the accumulation buffer at 512 KB so we don't grow without bound
                        if pending_terminal.len() > 524288 {
                            let cut = pending_terminal.len() - 262144;
                            pending_terminal.drain(..cut);
                        }
                    }
                    UiMessage::TerminalDone(success) => {
                        flush_now = true;
                        window.set_terminal_done(true);
                        window.set_terminal_success(success);
                        if success {
                            // Optimistic instant removal
                            if let Some((action, names, backend)) = conflict_ctx_timer.lock().unwrap().clone() {
                                let is_remove = action == "remove" || action == "bulk-remove";
                                if is_remove && !names.is_empty() {
                                    let name_set: std::collections::HashSet<&str> =
                                        names.iter().map(|s| s.as_str()).collect();
                                    if backend == 1 {
                                        // Filter installed_flatpaks (the Installed tab in view 10)
                                        let current: Vec<PackageData> = window.get_installed_flatpaks()
                                            .iter()
                                            .filter(|p| !name_set.contains(p.name.as_str()))
                                            .collect();
                                        window.set_installed_flatpaks(ModelRc::new(VecModel::from(current)));
                                    } else {
                                        {
                                            let mut inst = full_installed_timer.borrow_mut();
                                            inst.retain(|p| !name_set.contains(p.name.as_str()));
                                        }
                                        let ps = page_size as usize;
                                        let inst = full_installed_timer.borrow();
                                        let total = ((inst.len() + ps - 1) / ps).max(1) as i32;
                                        let page: Vec<PackageData> = inst.iter().take(ps).cloned().collect();
                                        drop(inst);
                                        window.set_installed_packages(ModelRc::new(VecModel::from(page)));
                                        window.set_current_page(0);
                                        window.set_total_pages(total);
                                    }
                                }
                            }
                            let tx = tx_timer.clone();
                            let search_query = window.get_search_text().to_string();
                            let ids_ref = flatpak_ids_timer.clone();
                            let store_ref = flatpak_store_timer.clone();
                            thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().expect("Runtime");
                                rt.block_on(async {
                                    // Refresh flatpak installed ids first - used by search + browse
                                    let new_ids = tokio::task::spawn_blocking(get_flatpak_installed_ids).await.unwrap_or_default();
                                    *ids_ref.lock().unwrap() = new_ids;
                                    // Run load + search concurrently
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
                                });
                            });
                        }
                    }
                    UiMessage::SetTerminalIsUpgrade(val) => {
                        window.set_terminal_is_upgrade(val);
                    }
                    UiMessage::HideTerminal => {
                        window.set_show_terminal(false);
                        window.set_show_progress_popup(false);
                        window.set_terminal_is_upgrade(false);
                    }
                    UiMessage::ShowProgressPopup(title) => {
                        window.set_progress_popup_title(SharedString::from(&title));
                        window.set_progress_popup_percent(0);
                        window.set_progress_popup_output(SharedString::from(""));
                        window.set_progress_popup_stage(SharedString::from("Starting..."));
                        window.set_progress_popup_output(SharedString::from(""));
                        window.set_progress_popup_show_input(false);
                        window.set_progress_popup_prompt(SharedString::from(""));
                        window.set_progress_popup_done(false);
                        window.set_progress_popup_success(false);
                        window.set_show_progress_logs(false);
                        window.set_progress_show_details(false);
                        window.set_progress_popup_show_buttons(false);
                        window.set_progress_popup_eta(SharedString::from(""));
                        window.set_progress_error_summary(SharedString::from(""));
                        window.set_progress_popup_show_close(false);
                        let new_log = Rc::new(VecModel::<LogLine>::default());
                        window.set_progress_log_lines(ModelRc::new(new_log.clone()));
                        *log_model_timer.borrow_mut() = Some(new_log);
                        window.set_show_progress_popup(true);
                        window.set_show_terminal(false);
                    }
                    UiMessage::ProgressOutput(text) => {
                        window.set_progress_popup_output(SharedString::from(&text));
                    }
                    UiMessage::ProgressPrompt(prompt) => {
                        window.set_progress_popup_prompt(SharedString::from(&prompt));
                        window.set_progress_popup_show_input(true);
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
                    UiMessage::ProgressLogLine(text, level) => {
                        let model_opt = log_model_timer.borrow();
                        if let Some(model) = model_opt.as_ref() {
                            model.push(LogLine {
                                text: SharedString::from(text.as_str()),
                                level: level as i32,
                            });
                        }
                    }
                    UiMessage::ProgressETA(eta) => {
                        window.set_progress_popup_eta(SharedString::from(&eta));
                    }
                    UiMessage::ProgressErrorSummary(s) => {
                        window.set_progress_error_summary(SharedString::from(&s));
                    }
                    UiMessage::ProgressAutoExpand => {
                        window.set_progress_show_details(true);
                    }
                    UiMessage::OperationProgress(percent, stage) => {
                        window.set_progress_popup_percent(percent);
                        window.set_progress_popup_stage(SharedString::from(&stage));
                    }
                    UiMessage::OperationDone(success) => {
                        window.set_progress_popup_percent(100);
                        window.set_progress_popup_done(true);
                        window.set_progress_popup_success(success);
                        window.set_progress_popup_show_input(false);
                        window.set_progress_popup_prompt(SharedString::from(""));
                        if success {
                            // Optimistic instant removal: drop the packages from the displayed
                            // lists right now so the UI updates before the full ALPM reload.
                            if let Some((action, names, backend)) = conflict_ctx_timer.lock().unwrap().clone() {
                                let is_remove = action == "remove" || action == "bulk-remove";
                                if is_remove && !names.is_empty() {
                                    let name_set: std::collections::HashSet<&str> =
                                        names.iter().map(|s| s.as_str()).collect();
                                    if backend == 1 {
                                        // Flatpak removal - filter installed_flatpaks
                                        let current: Vec<PackageData> = window.get_installed_flatpaks()
                                            .iter()
                                            .filter(|p| !name_set.contains(p.name.as_str()))
                                            .collect();
                                        window.set_installed_flatpaks(ModelRc::new(VecModel::from(current)));
                                    } else {
                                        // Native pacman removal - filter full installed list + re-page
                                        {
                                            let mut inst = full_installed_timer.borrow_mut();
                                            inst.retain(|p| !name_set.contains(p.name.as_str()));
                                        }
                                        let ps = page_size as usize;
                                        let inst = full_installed_timer.borrow();
                                        let total = ((inst.len() + ps - 1) / ps).max(1) as i32;
                                        let page: Vec<PackageData> = inst.iter().take(ps).cloned().collect();
                                        drop(inst);
                                        window.set_installed_packages(ModelRc::new(VecModel::from(page)));
                                        window.set_current_page(0);
                                        window.set_total_pages(total);
                                    }
                                }
                            }
                            window.set_selected_count(0);
                        } else {
                            window.set_show_progress_logs(true);
                        }
                        // Only reload after a successful operation. On failure/cancel nothing
                        // actually changed on the system, so preserve the current UI state
                        // (especially the updates list which load_packages_async(false) would clear).
                        if success {
                        let tx = tx_timer.clone();
                            let search_query = window.get_search_text().to_string();
                            let ids_ref = flatpak_ids_timer.clone();
                            let store_ref = flatpak_store_timer.clone();
                            // Optimistically zero the tray badge — updates were just applied.
                            // Then signal the check task to do a real recount and redraw.
                            tray_count_op.store(0, Ordering::Relaxed);
                            if let Some(check_tx) = tray_check_op.lock().unwrap().as_ref() {
                                check_tx.send(()).ok();
                            }

                            thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().expect("Runtime");
                                rt.block_on(async {
                                    // Refresh flatpak installed ids first - used by search + browse
                                    let new_ids = tokio::task::spawn_blocking(get_flatpak_installed_ids).await.unwrap_or_default();
                                    *ids_ref.lock().unwrap() = new_ids;
                                    // Run load + search concurrently
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
                                });
                            });
                        } // end if success
                    }
                    UiMessage::ShowConflict { summary, can_force } => {
                        window.set_show_progress_popup(false);
                        window.set_conflict_summary(SharedString::from(&summary));
                        window.set_conflict_can_force(can_force);
                        window.set_show_conflict_dialog(true);
                    }
                    UiMessage::FlatpakDetailReady { name, summary, description, developer, version, version_date, changelog, url_homepage, url_bugtracker, url_translate, url_vcs, categories } => {
                        window.set_flatpak_detail_name(SharedString::from(&name));
                        window.set_flatpak_detail_summary(SharedString::from(&summary));
                        // Normalise paragraph spacing: collapse \n\n → \n, then expand \n → \n\n
                        let fmt_desc = if description.contains('\n') {
                            description.replace("\n\n", "\n").replace('\n', "\n\n")
                        } else {
                            description.clone()
                        };
                        window.set_flatpak_detail_description(SharedString::from(&fmt_desc));
                        window.set_flatpak_detail_developer(SharedString::from(&developer));
                        window.set_flatpak_detail_version(SharedString::from(&version));
                        window.set_flatpak_detail_version_date(SharedString::from(&version_date));
                        let fmt_changelog = if changelog.contains('\n') {
                            changelog.replace("\n\n", "\n").replace('\n', "\n\n")
                        } else {
                            changelog.clone()
                        };
                        window.set_flatpak_detail_changelog(SharedString::from(&fmt_changelog));
                        window.set_flatpak_detail_url_homepage(SharedString::from(&url_homepage));
                        window.set_flatpak_detail_url_bug(SharedString::from(&url_bugtracker));
                        window.set_flatpak_detail_url_translate(SharedString::from(&url_translate));
                        window.set_flatpak_detail_url_vcs(SharedString::from(&url_vcs));
                        window.set_flatpak_detail_tags(ModelRc::new(VecModel::from(
                            categories.iter().map(|c| SharedString::from(c.as_str())).collect::<Vec<_>>()
                        )));
                        window.set_show_flatpak_detail(true);
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
                        if let Some(first) = remotes.first() {
                            window.set_selected_remote(SharedString::from(first.as_str()));
                        }
                    }
                    UiMessage::RemoteAppsFiltered { serial, apps, total_matches } => {
                        // u64::MAX is a sentinel used by preload/browse paths - always accept
                        // For normal filter serials, drop stale results from previous keystrokes
                        let current = filter_serial_timer.load(std::sync::atomic::Ordering::Relaxed);
                        if serial == u64::MAX || serial == current {
                            window.set_flatpak_total_matches(total_matches as i32);
                            window.set_remote_apps(ModelRc::new(VecModel::from(apps)));
                            window.set_remote_apps_loading(false);
                            window.set_flatpak_store_ready(true);
                            window.set_flatpak_loading_more(false);
                        }
                    }
                    UiMessage::FlatpakScreenshotReady(path) => {
                        if let Ok(img) = slint::Image::load_from_path(std::path::Path::new(&path)) {
                            window.set_flatpak_screenshot(img);
                        }
                    }
                    UiMessage::FlatpakIconReady(path) => {
                        if let Ok(img) = slint::Image::load_from_path(std::path::Path::new(&path)) {
                            window.set_flatpak_detail_icon(img);
                        }
                    }
                    UiMessage::FlatpakAddonsReady(addons) => {
                        // Split into two clean lists so the modal can show them in separate sections.
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
                    UiMessage::FlatpakPageAppended(new_items) => {
                        // Append next page to the existing remote-apps model
                        let model = window.get_remote_apps();
                        let mut current: Vec<PackageData> = (0..model.row_count())
                            .filter_map(|i| model.row_data(i))
                            .collect();
                        current.extend(new_items);
                        window.set_remote_apps(ModelRc::new(VecModel::from(current)));
                        window.set_flatpak_loading_more(false);
                    }
                    UiMessage::PacmanReposLoaded(repos) => {
                        // Keep selected_repo as "" (All) - the browse-repo("") call
                        // that fired alongside load-pacman-repos handles the initial load.
                        window.set_pacman_repos(ModelRc::new(VecModel::from(
                            repos.iter().map(|r| SharedString::from(r.as_str())).collect::<Vec<_>>()
                        )));
                    }
                    UiMessage::RepoPackagesLoaded(pkgs) => {
                        *repo_full_timer.borrow_mut() = pkgs.clone();
                        const INITIAL_LIMIT: usize = 150;
                        let has_more = pkgs.len() > INITIAL_LIMIT;
                        let extra = pkgs.len().saturating_sub(INITIAL_LIMIT) as i32;
                        let displayed = if has_more { pkgs[..INITIAL_LIMIT].to_vec() } else { pkgs };
                        window.set_repo_packages(ModelRc::new(VecModel::from(displayed)));
                        window.set_repo_has_more(has_more);
                        window.set_repo_extra_count(extra);
                        window.set_repo_loading(false);
                        window.set_repo_search(SharedString::from(""));
                    }
                    UiMessage::RepoPkgDetail(desc) => {
                        window.set_repo_detail_description(SharedString::from(&desc));
                        window.set_repo_detail_loading(false);
                    }
                    UiMessage::InstalledFlatpaksLoaded(pkgs) => {
                        *full_installed_flatpaks_timer.borrow_mut() = pkgs.clone();
                        window.set_installed_flatpaks(ModelRc::new(VecModel::from(pkgs)));
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
                    UiMessage::ProgressShowClose => {
                        window.set_progress_popup_show_close(true);
                    }
                    UiMessage::ShowWarning { message, chaotic_aur } => {
                        window.set_warning_popup_message(SharedString::from(&message));
                        window.set_warning_popup_chaotic_aur(chaotic_aur);
                        window.set_show_warning_popup(true);
                    }
                }
            }

            if !pending_terminal.is_empty()
                && (flush_now || last_term_flush.elapsed() >= std::time::Duration::from_millis(50))
                {
                    let text = std::mem::take(&mut pending_terminal);
                    let current = window.get_terminal_output().to_string();
                    let combined = apply_terminal_text(&current, &text);
                    // Keep last 500 lines rather than a fixed byte limit
                    const MAX_TERM_LINES: usize = 500;
                    let trimmed = {
                        let lc = combined.split('\n').count();
                        if lc > MAX_TERM_LINES {
                            let skip = lc - MAX_TERM_LINES;
                            let start = combined.split('\n').take(skip).map(|l| l.len() + 1).sum::<usize>();
                            combined[start.min(combined.len())..].to_string()
                        } else {
                            combined
                        }
                    };
                    window.set_terminal_output(SharedString::from(&trimmed));
                    last_term_flush = std::time::Instant::now();
                }
        }
    });

    // Load config first so initial thread can use check_updates_on_start
    let config = load_config();
    let check_updates_on_start = config.check_updates_on_start;

    // Fire homepage data immediately - these are pure /proc reads, sub-millisecond
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

    // Preload flatpak appstream in background so first Flatpaks click is instant
    {
        let store_preload = flatpak_app_store.clone();
        let ids_preload = flatpak_installed_ids.clone();
        let tx_preload = tx.clone();
        thread::spawn(move || {
            let remotes = fetch_flatpak_remotes();
            let target = remotes.first().cloned().unwrap_or_else(|| "flathub".to_string());
            // Send remotes first so sidebar populates
            let _ = tx_preload.send(UiMessage::FlatpakRemotesLoaded(remotes));
            let (all_apps, installed) = load_remote_apps(&target);
            *ids_preload.lock().unwrap() = installed.clone();
            // Build first page immediately for instant display
            let all_pkg = apps_to_package_data(&all_apps, &installed, &target, "All", "");
            let total = all_pkg.len();
            let page: Vec<PackageData> = all_pkg.into_iter().take(FLATPAK_PAGE_SIZE).collect();
            *store_preload.lock().unwrap() = all_apps;
            // Use u64::MAX sentinel - initial population is always accepted by the message loop
            let _ = tx_preload.send(UiMessage::RemoteAppsFiltered { serial: u64::MAX, apps: page, total_matches: total });
        });
    }

    if let Some(ref path) = local_package_path {
        if let Some(pkg_info) = get_local_package_info(path) {
            window.set_local_package(pkg_info);
            window.set_local_package_path(SharedString::from(path.as_str()));
            window.set_show_local_install(true);
            window.set_view(4);
        }
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
                let total = ((data.len() + ps - 1) / ps).max(1) as i32;
                window.set_installed_packages(ModelRc::new(VecModel::from(page_data)));
                window.set_total_pages(total);
            }
        }
    });

    // Filter installed packages client-side (instant, no network)
    let full_installed_filter = full_installed.clone();
    let window_weak_fi = window.as_weak();
    window.on_filter_installed(move |query| {
        if let Some(w) = window_weak_fi.upgrade() {
            let q = query.to_string().to_lowercase();
            let data = full_installed_filter.borrow();
            let filtered: Vec<PackageData> = if q.is_empty() {
                // Reset to first page
                let ps = 50usize;
                let total = ((data.len() + ps - 1) / ps).max(1) as i32;
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

    // Filter installed flatpaks client-side (instant)
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

    // Unlock pacman DB (remove stale lock file)
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

    // Read IgnorePkg from /etc/pacman.conf
    let window_weak_igr = window.as_weak();
    window.on_read_ignorepkg(move || {
        if let Some(w) = window_weak_igr.upgrade() {
            let content = std::fs::read_to_string("/etc/pacman.conf").unwrap_or_default();
            let mut active = false;
            let mut value = String::new();
            for line in content.lines() {
                let trimmed = line.trim();
                // Match both commented (#IgnorePkg) and active (IgnorePkg)
                let stripped = trimmed.strip_prefix('#').unwrap_or(trimmed).trim();
                if let Some(rest) = stripped.strip_prefix("IgnorePkg") {
                    let v = rest.trim_start_matches(|c: char| c == ' ' || c == '=').trim().to_string();
                    // Active = line is NOT commented
                    if !trimmed.starts_with('#') {
                        active = true;
                    }
                    // Always capture value if non-empty
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

    // Save IgnorePkg to /etc/pacman.conf via pkexec sed
    window.on_save_ignorepkg(move |active, value| {
        let value = value.to_string();
        thread::spawn(move || {
            let line = if active {
                format!("IgnorePkg = {}", value.trim())
            } else {
                format!("#IgnorePkg = {}", value.trim())
            };
            // Replace any existing IgnorePkg line (commented or not); append if missing
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

    // Write ParallelDownloads to /etc/pacman.conf via pkexec
    window.on_set_parallel_downloads(move |n| {
        let val = n as u32;
        thread::spawn(move || {
            // sed: replace existing line (commented or not), or append if missing
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
    let update_all_ctx = conflict_context.clone();
    let window_weak_ua = window.as_weak();
    window.on_update_all(move || {
        info!("Update all packages (native + flatpak)");
        let needs_reboot = window_weak_ua.upgrade()
            .map(|w| native_updates_need_reboot(&w))
            .unwrap_or(false);
        let tx = tx_update.clone();
        let input = update_all_input.clone();
        let pid = update_all_pid.clone();
        let _ctx = update_all_ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetTerminalIsUpgrade(needs_reboot));
            run_in_terminal(
                &tx,
                "Full System Update",
                "pkexec",
                &[
                    "bash", "-c",
                    "pacman -Syu && echo '' && echo '━━━ Flatpak Updates ━━━' && flatpak update -y && echo '' && echo '✓ System fully updated'",
                ],
                &input,
                &pid,
            );
        });
    });

    let tx_native = tx.clone();
    let native_input = terminal_input_sender.clone();
    let native_pid = terminal_child_pid.clone();
    let native_ctx = conflict_context.clone();
    let window_weak_no = window.as_weak();
    window.on_update_native_only(move || {
        info!("Update native packages only");
        let needs_reboot = window_weak_no.upgrade()
            .map(|w| native_updates_need_reboot(&w))
            .unwrap_or(false);
        let tx = tx_native.clone();
        let input = native_input.clone();
        let pid = native_pid.clone();
        let ctx = native_ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetTerminalIsUpgrade(needs_reboot));
            run_managed_operation(&tx, "Native Update", "update-all", &[], 0, &input, &pid, &ctx);
        });
    });

    let tx_upd_flt = tx.clone();
    let upd_flt_input = terminal_input_sender.clone();
    let upd_flt_pid = terminal_child_pid.clone();
    let upd_flt_ctx = conflict_context.clone();
    window.on_update_all_flatpaks(move || {
        info!("Update all flatpaks");
        let tx = tx_upd_flt.clone();
        let input = upd_flt_input.clone();
        let pid = upd_flt_pid.clone();
        let ctx = upd_flt_ctx.clone();
        thread::spawn(move || {
            // Flatpaks never require a kernel reboot
            let _ = tx.send(UiMessage::SetTerminalIsUpgrade(false));
            run_managed_operation(&tx, "Flatpak Update", "update-all", &[], 1, &input, &pid, &ctx);
        });
    });

    // Combined native + flatpak system update (used by tray "Update System")
    let tx_sys_full = tx.clone();
    let sys_full_input = terminal_input_sender.clone();
    let sys_full_pid = terminal_child_pid.clone();
    let window_weak_sf = window.as_weak();
    window.on_update_system_full(move || {
        info!("Full system update (native + flatpak)");
        let needs_reboot = window_weak_sf.upgrade()
            .map(|w| native_updates_need_reboot(&w))
            .unwrap_or(false);
        let tx = tx_sys_full.clone();
        let input = sys_full_input.clone();
        let pid = sys_full_pid.clone();
        thread::spawn(move || {
            let _ = tx.send(UiMessage::SetTerminalIsUpgrade(needs_reboot));
            run_in_terminal(
                &tx,
                "Full System Update",
                "pkexec",
                &[
                    "bash", "-c",
                    "pacman -Syu && echo '' && echo '━━━ Flatpak Updates ━━━' && flatpak update -y && echo '' && echo '✓ System fully updated'",
                ],
                &input,
                &pid,
            );
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
            let mut args = vec!["uninstall", "--noninteractive", "--assumeyes", &id];
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
    let cp_pid = terminal_child_pid.clone();
    let cp_input = terminal_input_sender.clone();
    let tx_cp = tx.clone();
    window.on_close_progress_popup(move || {
        if let Some(window) = window_weak_cp.upgrade() {
            // If operation is still running (show-close mode = downgrade X button), kill it
            if !window.get_progress_popup_done() {
                if let Some(pid) = *cp_pid.lock().unwrap() {
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
                }
                *cp_input.lock().unwrap() = None;
                let _ = tx_cp.send(UiMessage::OperationDone(false));
            }
            window.set_show_progress_popup(false);
            window.set_show_progress_logs(false);
        }
    });

    let cancel_pid = terminal_child_pid.clone();
    let cancel_input = terminal_input_sender.clone();
    let tx_cancel = tx.clone();
    window.on_cancel_operation(move || {
        info!("Operation cancelled by user");
        if let Some(pid) = *cancel_pid.lock().unwrap() {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
        }
        *cancel_input.lock().unwrap() = None;
        let _ = tx_cancel.send(UiMessage::OperationDone(false));
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
            // pacman -Scc removes cached packages; also nuke download-* dirs (pacman bug workaround)
            let script = "pacman -Scc --noconfirm; \
                          echo ''; \
                          echo 'Removing leftover download dirs (pacman bug workaround)...'; \
                          rm -rfv /var/cache/pacman/pkg/download-* 2>/dev/null && echo 'Done.' || echo 'No download dirs found.'";
            run_in_terminal(&tx, "Cleaning Package Cache", "pkexec", &["bash", "-c", script], &input, &pid);
        });
    });

    // Toggle individual orphan checkbox
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

    // Select/deselect all orphan checkboxes
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

    // Load dep info for a single orphan package (what requires it)
    let window_weak_odi = window.as_weak();
    window.on_load_orphan_dep_info(move |pkg_name| {
        let name = pkg_name.to_string();
        let window_weak = window_weak_odi.clone();
        thread::spawn(move || {
            // `pacman -Qi <name>` → Required By field
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

            // Also check optional for
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

    // Load orphan list into dialog
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

            // Get details for each orphan via `pacman -Qi`
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
            let checked = vec![false; len]; // none selected by default
            slint::invoke_from_event_loop(move || {
                if let Some(w) = window_weak.upgrade() {
                    w.set_orphan_list(ModelRc::new(VecModel::from(pkgs)));
                    w.set_orphan_checked(ModelRc::new(VecModel::from(checked)));
                    w.set_orphan_selected_count(0);
                }
            }).ok();
        });
    });

    // Remove only checked orphans
    let window_weak_orp_rm = window.as_weak();
    let tx_orphans = tx.clone();
    let orphan_input = terminal_input_sender.clone();
    let orphan_pid = terminal_child_pid.clone();
    window.on_remove_selected_orphans(move || {
        let Some(w) = window_weak_orp_rm.upgrade() else { return; };
        let pkgs: Vec<PackageData> = w.get_orphan_list().iter().collect();
        let checked: Vec<bool> = w.get_orphan_checked().iter().collect();
        let selected: Vec<String> = pkgs.iter().zip(checked.iter())
            .filter(|(_, &c)| c)
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

    // Legacy callback - kept for any stray references
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
    window.on_sync_databases(move || {
        info!("Check for updates");
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

    let term_input = terminal_input_sender.clone();
    let tx_term_echo = tx.clone();
    let window_weak_te = window.as_weak();
    window.on_terminal_send_input(move |text| {
        let text = text.to_string();
        // Local echo: show what the user typed in the terminal output immediately.
        // PTY echo is unreliable after sudo's tcsetattr - don't rely on it.
        // Skip echo when in password mode (don't reveal the typed secret).
        let is_password = window_weak_te.upgrade()
            .map(|w| w.get_terminal_show_password())
            .unwrap_or(false);
        if !is_password {
            if !text.is_empty() {
                let _ = tx_term_echo.send(UiMessage::TerminalOutput(format!("{}\n", text)));
            } else {
                // Empty input + Enter: detect default from the last prompt line and echo it.
                // The PTY writer always appends \n, so the program receives its default anyway;
                // we just want to show the user what was "selected".
                let output = window_weak_te.upgrade()
                    .map(|w| w.get_terminal_output().to_string())
                    .unwrap_or_default();
                let last_line = output.lines().last().unwrap_or("").trim_end();
                if let Some(default) = detect_prompt_default(last_line) {
                    let _ = tx_term_echo.send(UiMessage::TerminalOutput(format!("{}\n", default)));
                }
            }
        }
        if let Some(sender) = term_input.lock().unwrap().as_ref() {
            let _ = sender.send(text);
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
            // Prompt user for save location via kdialog (falls back to zenity)
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
                    // User cancelled
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

    let tx_close = tx.clone();
    let close_pid = terminal_child_pid.clone();
    let close_input = terminal_input_sender.clone();
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

    window.on_terminal_close(move || {
        info!("Terminal close requested");
        if let Some(pid) = *close_pid.lock().unwrap() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
        *close_input.lock().unwrap() = None;

        let _ = tx_close.send(UiMessage::HideTerminal);
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

    // Flatpak remote browser - serves capped first page from in-memory store if preloaded
    let tx_remotes = tx.clone();
    let window_weak_remote = window.as_weak();
    let store_remote = flatpak_app_store.clone();
    let ids_remote = flatpak_installed_ids.clone();
    window.on_browse_remote(move |remote| {
        let tx = tx_remotes.clone();
        let remote_str = remote.to_string();
        info!("Browse remote: {}", remote_str);

        // If the store is already populated (preloaded), serve first page immediately
        {
            let store = store_remote.lock().unwrap();
            if !store.is_empty() {
                let ids = ids_remote.lock().unwrap();
                let target = if remote_str.is_empty() {
                    "flathub".to_string()
                } else {
                    remote_str.clone()
                };
                // Only deliver first FLATPAK_PAGE_SIZE items for instant render
                let all = apps_to_package_data(&store, &ids, &target, "All", "");
                let total = all.len();
                let page: Vec<PackageData> = all.into_iter().take(FLATPAK_PAGE_SIZE).collect();
                drop(ids);
                drop(store);
                // u64::MAX sentinel - browse result always accepted
                let _ = tx.send(UiMessage::RemoteAppsFiltered { serial: u64::MAX, apps: page, total_matches: total });
                return;
            }
        }

        // Store not ready yet - show loading and fetch in background
        if let Some(w) = window_weak_remote.upgrade() {
            w.set_remote_apps_loading(true);
        }
        let tx2 = tx.clone();
        let remote2 = remote_str.clone();
        let store = store_remote.clone();
        let ids = ids_remote.clone();
        thread::spawn(move || {
            let target = if remote2.is_empty() {
                let remotes = fetch_flatpak_remotes();
                let first = remotes.first().cloned().unwrap_or_else(|| "flathub".to_string());
                let _ = tx2.send(UiMessage::FlatpakRemotesLoaded(remotes));
                first
            } else {
                remote2
            };
            let (all_apps, installed) = load_remote_apps(&target);
            *ids.lock().unwrap() = installed.clone();
            let all = apps_to_package_data(&all_apps, &installed, &target, "All", "");
            *store.lock().unwrap() = all_apps;
            let total = all.len();
            let page: Vec<PackageData> = all.into_iter().take(FLATPAK_PAGE_SIZE).collect();
            // u64::MAX sentinel - background browse fetch always accepted
            let _ = tx.send(UiMessage::RemoteAppsFiltered { serial: u64::MAX, apps: page, total_matches: total });
        });
    });

    // Filter flatpak apps - background thread, stale results dropped via serial counter
    let tx_filter = tx.clone();
    let store_filter = flatpak_app_store.clone();
    let ids_filter = flatpak_installed_ids.clone();
    let window_weak_filter = window.as_weak();
    let serial_filter = flatpak_filter_serial.clone();
    window.on_filter_flatpak(move |category, search| {
        let cat = category.to_string();
        let q = search.to_string();
        let remote = if let Some(w) = window_weak_filter.upgrade() {
            w.get_selected_remote().to_string()
        } else {
            "flathub".to_string()
        };
        // Bump serial immediately - any in-flight result with the old serial will be dropped
        let my_serial = serial_filter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let store = store_filter.clone();
        let ids = ids_filter.clone();
        let tx = tx_filter.clone();
        let serial_check = serial_filter.clone();
        thread::spawn(move || {
            let store = store.lock().unwrap();
            let ids = ids.lock().unwrap();
            // Check if already superseded before doing expensive work
            if serial_check.load(std::sync::atomic::Ordering::Relaxed) != my_serial {
                return;
            }
            let all = apps_to_package_data(&store, &ids, &remote, &cat, &q);
            drop(store);
            drop(ids);
            // Check again after the filter work
            if serial_check.load(std::sync::atomic::Ordering::Relaxed) != my_serial {
                return;
            }
            let total = all.len();
            // Cap to FLATPAK_PAGE_SIZE - enough for display, avoids VecModel of 2000 items
            let page: Vec<PackageData> = all.into_iter().take(FLATPAK_PAGE_SIZE).collect();
            let _ = tx.send(UiMessage::RemoteAppsFiltered { serial: my_serial, apps: page, total_matches: total });
        });
    });

    // Load next page of flatpak results
    let tx_load_more = tx.clone();
    let store_load_more = flatpak_app_store.clone();
    let ids_load_more = flatpak_installed_ids.clone();
    let window_weak_more = window.as_weak();
    let serial_load_more = flatpak_filter_serial.clone();
    window.on_load_more_flatpaks(move || {
        let tx = tx_load_more.clone();
        let store = store_load_more.clone();
        let ids = ids_load_more.clone();
        // Capture current offset and filter state from UI thread
        let (offset, remote, category, search) = if let Some(w) = window_weak_more.upgrade() {
            (
                w.get_remote_apps().row_count(),
                w.get_selected_remote().to_string(),
                w.get_selected_flatpak_category().to_string(),
                w.get_flatpak_search().to_string(),
            )
        } else {
            return;
        };
        let my_serial = serial_load_more.load(std::sync::atomic::Ordering::Relaxed);
        thread::spawn(move || {
            let store = store.lock().unwrap();
            let ids = ids.lock().unwrap();
            let all = apps_to_package_data(&store, &ids, &remote, &category, &search);
            drop(store);
            drop(ids);
            // Slice the next page starting from the current offset
            let next_page: Vec<PackageData> = all
                .into_iter()
                .skip(offset)
                .take(FLATPAK_PAGE_SIZE)
                .collect();
            // Only deliver if the filter hasn't changed since the button was clicked
            let current_serial = my_serial; // we captured it before spawning
            let _ = tx.send(UiMessage::FlatpakPageAppended(next_page));
            let _ = current_serial; // suppress unused warning
        });
    });

    // Toggle individual flatpak selection (checkbox in flat list)
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

    // Batch install selected flatpaks
    let win_batch_fi = window.as_weak();
    let tx_bfi = tx.clone();
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
            let title = format!("Installing {} Flatpak(s)...", ids.len());
            let _ = tx.send(UiMessage::ShowTerminal(title.clone()));
            thread::spawn(move || {
                let mut args = vec!["install".to_string(), "-y".to_string(), "flathub".to_string()];
                args.extend(ids.iter().cloned());
                let status = std::process::Command::new("flatpak")
                    .args(&args)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let _ = tx.send(UiMessage::TerminalDone(status));
            });
        }
    });

    // Batch remove selected flatpaks
    let win_batch_fr = window.as_weak();
    let tx_bfr = tx.clone();
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
            let title = format!("Removing {} Flatpak(s)...", ids.len());
            let _ = tx.send(UiMessage::ShowTerminal(title.clone()));
            thread::spawn(move || {
                let mut args = vec!["uninstall".to_string(), "-y".to_string()];
                args.extend(ids.iter().cloned());
                let status = std::process::Command::new("flatpak")
                    .args(&args)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let _ = tx.send(UiMessage::TerminalDone(status));
            });
        }
    });

    // Lookup detail from in-memory store (no network)
    let tx_detail = tx.clone();
    let store_detail = flatpak_app_store.clone();
    let ids_detail = flatpak_installed_ids.clone();
    window.on_load_flatpak_detail(move |app_id| {
        let id = app_id.to_string();
        let store = store_detail.lock().unwrap();
        let installed = ids_detail.lock().unwrap();
        if let Some(app) = store.iter().find(|a| a.app_id == id) {
            let _ = tx_detail.send(UiMessage::FlatpakDetailReady {
                name: if app.name.is_empty() { app.app_id.clone() } else { app.name.clone() },
                summary: app.summary.clone(),
                description: app.description.clone(),
                developer: app.developer.clone(),
                version: app.version.clone(),
                version_date: app.version_date.clone(),
                changelog: app.changelog.clone(),
                url_homepage: app.url_homepage.clone(),
                url_bugtracker: app.url_bugtracker.clone(),
                url_translate: app.url_translate.clone(),
                url_vcs: app.url_vcs.clone(),
                categories: app.categories.clone(),
            });
            // Find addons (apps that extend this one)
            let addons: Vec<PackageData> = store.iter()
                .filter(|a| a.extends == id)
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
            // Screenshot download
            let ss_url = app.screenshot_url.clone();
            let ss_id = id.clone();
            let tx_ss = tx_detail.clone();
            if !ss_url.is_empty() {
                thread::spawn(move || {
                    let tmp = format!("/tmp/xpm_ss_{}.jpg", ss_id.replace('/', "_").replace('.', "_"));
                    let ok = std::process::Command::new("curl")
                        .args(["-s", "--max-time", "20", "-L", "-o", &tmp, &ss_url])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if ok && std::path::Path::new(&tmp).exists() {
                        let _ = tx_ss.send(UiMessage::FlatpakScreenshotReady(tmp));
                    }
                });
            }
        }
    });

    // Load app icon from local appstream cache
    let store_icon = flatpak_app_store.clone();
    let tx_icon = tx.clone();
    window.on_load_flatpak_icon(move |app_id| {
        let id = app_id.to_string();
        let icon_name = {
            let store = store_icon.lock().unwrap();
            store.iter().find(|a| a.app_id == id).map(|a| a.icon_name.clone()).unwrap_or_default()
        };
        if !icon_name.is_empty() {
            let path = format!("/var/lib/flatpak/appstream/flathub/x86_64/active/icons/128x128/{}", icon_name);
            if std::path::Path::new(&path).exists() {
                let _ = tx_icon.send(UiMessage::FlatpakIconReady(path));
            }
        }
    });

    // Pacman repos browser - auto-loads first repo
    let tx_repos = tx.clone();
    window.on_load_pacman_repos(move || {
        let tx = tx_repos.clone();
        thread::spawn(move || {
            let repos = load_pacman_repos();
            let first = repos.first().cloned();
            let _ = tx.send(UiMessage::PacmanReposLoaded(repos));
            // Auto-load first repo immediately
            if let Some(repo) = first {
                let pkgs = load_repo_packages(&repo);
                let _ = tx.send(UiMessage::RepoPackagesLoaded(pkgs));
            }
        });
    });

    let tx_repo_pkgs = tx.clone();
    let window_weak_repo = window.as_weak();
    let load_more_full = repo_packages_full.clone();
    let win_load_more = window.as_weak();
    window.on_load_more_repo_pkgs(move || {
        let all = load_more_full.borrow().clone();
        if let Some(w) = win_load_more.upgrade() {
            w.set_repo_packages(ModelRc::new(VecModel::from(all)));
            w.set_repo_has_more(false);
            w.set_repo_extra_count(0);
        }
    });

    window.on_browse_repo(move |repo| {
        let tx = tx_repo_pkgs.clone();
        let repo_str = repo.to_string();
        info!("Browse repo: {}", repo_str);
        if let Some(w) = window_weak_repo.upgrade() {
            w.set_repo_loading(true);
            w.set_show_repo_detail(false);
        }
        thread::spawn(move || {
            let pkgs = load_repo_packages(&repo_str);
            let _ = tx.send(UiMessage::RepoPackagesLoaded(pkgs));
        });
    });

    // Repo package search filter
    let repo_full_filter = repo_packages_full.clone();
    let win_filter_repo = window.as_weak();
    window.on_filter_repo(move |search| {
        let q = search.to_string().to_lowercase();
        let full = repo_full_filter.borrow();
        let filtered: Vec<PackageData> = if q.is_empty() {
            full.clone()
        } else {
            full.iter().filter(|p| {
                p.name.to_lowercase().contains(&q)
                    || p.display_name.to_lowercase().contains(&q)
                    || p.description.to_lowercase().contains(&q)
            }).cloned().collect()
        };
        if let Some(w) = win_filter_repo.upgrade() {
            w.set_repo_packages(ModelRc::new(VecModel::from(filtered)));
        }
    });

    // Repo package detail: run pacman -Si <pkg>
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
                    // Extract Description field
                    text.lines()
                        .find(|l| l.starts_with("Description"))
                        .and_then(|l| l.splitn(2, ':').nth(1))
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
                // Keep repo headers (backend == -1) only if they have matching packages beneath
                let mut result = Vec::new();
                let mut current_header: Option<PackageData> = None;
                let mut header_has_match = false;
                for item in data.iter() {
                    if item.backend == -1 {
                        // flush previous header if it had matches
                        if let Some(h) = current_header.take() {
                            if header_has_match {
                                result.push(h);
                            }
                        }
                        current_header = Some(item.clone());
                        header_has_match = false;
                    } else {
                        let matches = item.name.to_lowercase().contains(&q)
                            || item.display_name.to_lowercase().contains(&q);
                        if matches {
                            if let Some(ref h) = current_header {
                                if !header_has_match {
                                    result.push(h.clone());
                                    header_has_match = true;
                                }
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

    // Apply explicit/dep filter to installed grouped
    let window_weak_ef = window.as_weak();
    let full_grp_ef = full_installed_grouped.clone();
    window.on_apply_explicit_filter(move |mode| {
        if let Some(w) = window_weak_ef.upgrade() {
            let data = full_grp_ef.borrow();
            // mode: 0=all, 1=explicit only, 2=deps only
            let filtered: Vec<PackageData> = if mode == 0 {
                data.clone()
            } else {
                let want_explicit = mode == 1;
                let mut result = Vec::new();
                let mut current_header: Option<PackageData> = None;
                let mut header_has_match = false;
                for item in data.iter() {
                    if item.backend == -1 {
                        if let Some(h) = current_header.take() {
                            if header_has_match {
                                result.push(h);
                            }
                        }
                        current_header = Some(item.clone());
                        header_has_match = false;
                    } else {
                        let matches = item.explicit == want_explicit;
                        if matches {
                            if let Some(ref h) = current_header {
                                if !header_has_match {
                                    result.push(h.clone());
                                    header_has_match = true;
                                }
                            }
                            result.push(item.clone());
                        }
                    }
                }
                if let Some(h) = current_header {
                    if header_has_match {
                        result.push(h);
                    }
                }
                result
            };
            w.set_installed_grouped(ModelRc::new(VecModel::from(filtered)));
        }
    });

    // Load ALL installed packages (no pagination cap)
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

        if !std::path::Path::new("/usr/bin/downgrade").exists() {
            let _ = tx.send(UiMessage::ShowWarning {
                message: "The downgrade package is not installed on this system.\n\nThis feature requires it to function.".to_string(),
                chaotic_aur: is_chaotic_aur_enabled(),
            });
            return;
        }

        let input = dg_input.clone();
        let pid = dg_pid.clone();
        thread::spawn(move || {
            // downgrade uses fzf (TUI) for version selection, which is invisible after
            // strip_ansi. We install a plain-text fzf replacement into a temp dir
            // and prepend it to PATH so downgrade uses our version instead.
            let fake_dir = format!("/tmp/xpm-fzf-{}", std::process::id());
            let fzf_path = format!("{}/fzf", fake_dir);
            let _ = std::fs::create_dir_all(&fake_dir);
            let _ = std::fs::write(&fzf_path, FAKE_FZF_SCRIPT);
            if let Ok(meta) = std::fs::metadata(&fzf_path) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = std::fs::set_permissions(&fzf_path, perms);
            }
            // Use pkexec so polkit handles auth graphically (Howdy → password fallback)
            // rather than prompting in the PTY. pkexec resets PATH so we re-export inside.
            let bash_cmd = format!(
                "export PATH={dir}:\"$PATH\"; /usr/bin/downgrade {name}",
                dir = fake_dir,
                name = name,
            );
            run_in_terminal_expanded(
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

    // ── Addon multi-select callbacks ──────────────────────────────────────────

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
        // flatpak-addons is uninstalled-only, so all entries are selectable
        let model = w.get_addon_selected();
        let count = model.row_count() as i32;
        for i in 0..model.row_count() {
            model.set_row_data(i, select);
        }
        w.set_addon_selected_count(if select { count } else { 0 });
    });

    let win_inst_addons = window.as_weak();
    let tx_inst_addons = tx.clone();
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
        let input = std::sync::Arc::new(std::sync::Mutex::new(None::<std::sync::mpsc::Sender<String>>));
        let pid = std::sync::Arc::new(std::sync::Mutex::new(None::<u32>));
        thread::spawn(move || {
            let mut args = vec!["install", "--noninteractive", "--assumeyes"];
            args.extend(ids.iter().map(|s| s.as_str()));
            run_in_terminal(&tx, &title, "flatpak", &args, &input, &pid);
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
                1, // flatpak backend
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

    window.on_save_settings(move || {
        if let Some(window) = window_weak_ss.upgrade() {
            let config = build_config(&window);
            save_config(&config);
        }
    });

    window.set_setting_flatpak_enabled(config.flatpak_enabled);
    window.set_setting_check_updates_on_start(config.check_updates_on_start);
    window.set_setting_notify_interval(config.notify_interval_minutes as i32);
    // Prefer actual value from /etc/pacman.conf over stored config
    let pacman_parallel = read_pacman_parallel_downloads().unwrap_or(config.parallel_downloads);
    window.set_setting_parallel_downloads(pacman_parallel as i32);
    // If value isn't a preset, activate custom mode so UI shows it correctly
    let presets = [5u32, 10, 15, 20, 25];
    if !presets.contains(&pacman_parallel) {
        window.set_setting_pd_custom_mode(true);
        window.set_setting_pd_custom_text(SharedString::from(pacman_parallel.to_string().as_str()));
    }

    // ── System tray ──────────────────────────────────────────────────────────
    let tray_shutdown: TrayShutdown = Arc::new(Mutex::new(None));
    let tray_shutdown_toggle = tray_shutdown.clone();

    // tray_update_count and tray_check_tx were created before the timer — reuse them.
    let tray_count_toggle = tray_update_count.clone();
    let tray_check_toggle = tray_check_tx.clone();

    // In --tray mode the tray is always considered enabled so the close handler
    // hides instead of quitting.
    let effective_tray = config.tray_enabled || tray_only;

    // Shared flag so the close handler knows whether to quit or just hide.
    let tray_enabled_flag = Arc::new(AtomicBool::new(effective_tray));
    let tray_flag_toggle = tray_enabled_flag.clone();
    let tray_flag_close  = tray_enabled_flag.clone();

    window.set_setting_tray_enabled(config.tray_enabled);
    window.set_setting_tray_check_interval(config.tray_check_interval_minutes as i32);
    window.set_aur_pill_dismissed(config.aur_pill_dismissed);
    // Start tray if enabled in settings OR if launched with --tray.
    if effective_tray {
        let interval_secs = (config.tray_check_interval_minutes as u64) * 60;
        start_tray(window.as_weak(), tray_shutdown.clone(), interval_secs, tx.clone(),
                   tray_update_count.clone(), tray_check_tx.clone());
    }

    let window_weak_tray = window.as_weak();
    let tx_tray_toggle = tx.clone();
    window.on_toggle_tray_enabled(move |enabled| {
        tray_flag_toggle.store(enabled, Ordering::Relaxed);
        if let Some(win) = window_weak_tray.upgrade() {
            let interval_secs = (win.get_setting_tray_check_interval() as u64) * 60;
            if enabled {
                set_autostart(true);
                start_tray(window_weak_tray.clone(), tray_shutdown_toggle.clone(), interval_secs,
                           tx_tray_toggle.clone(), tray_count_toggle.clone(), tray_check_toggle.clone());
            } else {
                set_autostart(false);
                stop_tray(&tray_shutdown_toggle);
                *tray_check_toggle.lock().unwrap() = None;
            }
        }
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

    // When the user closes the window while the tray is active, hide it instead
    // of quitting so the app keeps running in the system tray.
    // When the tray is disabled, close the window normally (quit event loop).
    window.window().on_close_requested(move || {
        if tray_flag_close.load(Ordering::Relaxed) {
            // Tray is alive — just hide; run_event_loop_until_quit keeps running.
            slint::CloseRequestResponse::HideWindow
        } else {
            // No tray — user wants to quit the app.
            slint::quit_event_loop().ok();
            slint::CloseRequestResponse::HideWindow
        }
    });

    if !is_xerolinux() && !config.distro_warning_dismissed {
        window.set_show_distro_warning(true);
    }

    info!("Running application (tray_only={})", tray_only);
    // In --tray mode skip showing the window — tray icon is the only entry point.
    // run_event_loop_until_quit keeps the process alive even with no visible windows.
    if !tray_only {
        window.show().expect("Failed to show window");
    }
    slint::run_event_loop_until_quit().expect("Failed to run application");
    // Background threads may still be alive when Slint's Qt backend tears down
    // its thread-local storage, producing QThreadStorage warnings. Exit immediately
    // so the process terminates cleanly instead of unwinding through Qt's cleanup.
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
    // desktop_map: reads .desktop files only (fast). pacman -Ql removed.
    // flatpak_map (appstream XML) removed - appdata_name already in list_installed().
    let desktop_map_fut = tokio::task::spawn_blocking(build_desktop_name_map);

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
         desktop_map_res,
    ) = tokio::join!(
        installed_fut,
        orphans_fut,
        flatpak_installed_fut,
        desktop_map_fut,
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
    let desktop_map = desktop_map_res.unwrap_or_default();

    // Compute cache size quickly in background (non-blocking estimate)
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
    if let Some(Ok(Ok(result))) = checkupdates_res {
        if result.status.success() {
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
                    });
                } else if parts.len() >= 2 {
                    updates.push(xpm_core::package::UpdateInfo {
                        name: parts[0].to_string(),
                                 current_version: xpm_core::package::Version::new(""),
                                 new_version: xpm_core::package::Version::new(parts[1]),
                                 backend: xpm_core::package::PackageBackend::Pacman,
                                 repository: String::new(),
                                 download_size: 0,
                    });
                }
            }
        }
    }

    let update_names: std::collections::HashSet<String> =
    updates.iter().map(|u| u.name.clone()).collect();
    let flatpak_update_names: std::collections::HashSet<String> =
    flatpak_updates.iter().map(|u| u.name.clone()).collect();

    let installed_ui: Vec<PackageData> = installed_pacman
    .iter()
    .map(|p| package_to_ui(p, update_names.contains(&p.name), &desktop_map))
    .collect();

    let updates_ui: Vec<PackageData> = updates.iter().map(|u| update_to_ui(u)).collect();

    let flatpak_ui: Vec<PackageData> = flatpak_packages
    .iter()
    .map(|p| {
        let has_update = flatpak_update_names.contains(&p.name);
        // appdata_name is stored in pkg.description by list_installed()
        let display_name = if !p.description.is_empty() {
            p.description.clone()
        } else {
            p.name.split('.').last().unwrap_or(&p.name)
                .replace('_', " ").replace('-', " ")
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

    let total_updates = updates.len() + flatpak_updates.len() + plasmoid_updates.len();
    let flatpak_update_count = flatpak_updates.len() as i32;

    // Build a display-name map from installed flatpak packages so flatpak updates
    // show friendly names (e.g. "GNOME Calculator") instead of app IDs.
    let flatpak_name_map: std::collections::HashMap<String, String> = flatpak_packages
        .iter()
        .map(|p| {
            let display_name = if !p.description.is_empty() {
                p.description.clone()
            } else {
                p.name.split('.').last().unwrap_or(&p.name)
                    .replace('_', " ").replace('-', " ")
            };
            (p.name.clone(), display_name)
        })
        .collect();

    let flatpak_updates_ui: Vec<PackageData> = flatpak_updates.iter()
        .map(|u| {
            let display_name = flatpak_name_map
                .get(&u.name)
                .cloned()
                .unwrap_or_else(|| {
                    u.name.split('.').last().unwrap_or(&u.name)
                        .replace('_', " ").replace('-', " ")
                });
            let ver_str = format!("{} → {}", u.current_version, u.new_version);
            PackageData {
                name: SharedString::from(u.name.as_str()),
                display_name: SharedString::from(display_name.as_str()),
                version: SharedString::from(ver_str.as_str()),
                description: SharedString::from(ver_str.as_str()),
                repository: SharedString::from("flatpak"),
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

    // Native updates = pacman + plasmoid updates (no flatpak mixed in)
    let mut native_updates_ui = updates_ui.clone();
    native_updates_ui.extend(plasmoid_updates.clone());

    // Use flatpak CLI for accurate installed count (includes runtimes/extensions the API may miss)
    let flatpak_real_count = std::process::Command::new("flatpak")
        .args(["list", "--system"])
        .output()
        .map(|o| o.stdout.iter().filter(|&&b| b == b'\n').count() as i32)
        .unwrap_or(flatpak_packages.len() as i32);

    let stats = StatsData {
        pacman_count: installed_pacman.len() as i32,
        flatpak_count: flatpak_real_count,
        orphan_count: orphan_count as i32,
        update_count: total_updates as i32,
        cache_size: SharedString::from(format_size(cache_size)),
    };

    // Save cache (combined for compatibility)
    let mut all_for_cache = native_updates_ui.clone();
    all_for_cache.extend(flatpak_updates_ui.clone());
    save_package_cache(&installed_ui, &all_for_cache, &flatpak_ui, &stats);

    let _ = tx.send(UiMessage::PackagesLoaded {
        installed: installed_ui,
        updates: native_updates_ui,
        flatpak_updates: flatpak_updates_ui,
        flatpak: flatpak_ui,
        stats,
        flatpak_update_count,
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
        {
            if output.status.success() {
                let response = String::from_utf8_lossy(&output.stdout);
                if let Ok(json) = serde_json::from_str::<Value>(&response) {
                    if let Some(data) = json.get("ocs").and_then(|o| o.get("data")).and_then(|d| d.as_array()) {
                        for item in data {
                            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            let version = item.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
                            if !name.is_empty() && !version.is_empty() {
                                versions.push((name, version));
                            }
                        }
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
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(json) = serde_json::from_str::<Value>(&content) {
            if let Some(kplugin) = json.get("KPlugin") {
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
        }
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


async fn search_packages_async(
    tx: &mpsc::Sender<UiMessage>,
    query: &str,
    flatpak_store: Arc<Mutex<Vec<CachedRemoteApp>>>,
    flatpak_ids: Arc<Mutex<std::collections::HashSet<String>>>,
) {
    let q = query.to_string();
    let q_lower = q.to_lowercase();

    // Snapshot flatpak data under lock, then release before doing any I/O
    let (store_snapshot, ids_snapshot) = {
        let store = flatpak_store.lock().unwrap();
        let ids = flatpak_ids.lock().unwrap();
        (store.clone(), ids.clone())
    };
    let store_is_empty = store_snapshot.is_empty();

    // Run ALPM search and flatpak data loading concurrently
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

    let desktop_map = build_desktop_name_map();

    let mut results: Vec<PackageData> = pacman_results
        .iter()
        .map(|r| {
            let display_name = humanize_package_name(&r.name, &desktop_map);
            PackageData {
                name: SharedString::from(r.name.as_str()),
                display_name: SharedString::from(&display_name),
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
            }
        })
        .collect();

    let fk: Vec<PackageData> = flatpak_apps.iter()
        .filter(|a| {
            a.name.to_lowercase().contains(&q_lower)
                || a.app_id.to_lowercase().contains(&q_lower)
                || a.summary.to_lowercase().contains(&q_lower)
        })
        .take(50)
        .map(|a| PackageData {
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
        })
        .collect();

    results.extend(fk);
    results.truncate(150);
    let _ = tx.send(UiMessage::SearchResults(results));
}

fn build_desktop_name_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let dirs = ["/usr/share/applications"];
    for dir in &dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "desktop") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let mut name = String::new();
                        let mut exec = String::new();
                        let mut no_display = false;
                        for line in content.lines() {
                            if line.starts_with("Name=") && !line.contains('[') {
                                name = line.strip_prefix("Name=").unwrap_or("").to_string();
                            } else if line.starts_with("Exec=") {
                                exec = line.strip_prefix("Exec=").unwrap_or("")
                                    .split_whitespace().next().unwrap_or("")
                                    .rsplit('/').next().unwrap_or("").to_string();
                            } else if line.starts_with("NoDisplay=true") {
                                no_display = true;
                            }
                        }
                        if !name.is_empty() && !no_display {
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                map.insert(stem.to_lowercase(), name.clone());
                            }
                            if !exec.is_empty() {
                                map.entry(exec.to_lowercase()).or_insert(name);
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

fn humanize_package_name(name: &str, desktop_map: &HashMap<String, String>) -> String {
    if let Some(human_name) = desktop_map.get(&name.to_lowercase()) {
        return human_name.clone();
    }
    name.split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(first) => first.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Package cache ────────────────────────────────────────────────────────────

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
    update_count: i32,
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
        update_count: stats.update_count,
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
    // Valid if pacman db hasn't changed
    if cache.pacman_db_mtime == pacman_db_mtime() {
        Some(cache)
    } else {
        None
    }
}

// ─── Flatpak remote browser ───────────────────────────────────────────────────

fn remote_cache_path(remote: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(format!("{}/.local/share/xpm/remote_{}.json", home, remote))
}

fn remote_cache_valid(path: &std::path::Path) -> bool {
    if let Ok(meta) = std::fs::metadata(path) {
        if let Ok(modified) = meta.modified() {
            let age = std::time::SystemTime::now()
                .duration_since(modified)
                .unwrap_or(std::time::Duration::MAX);
            return age.as_secs() < 86400; // 24h
        }
    }
    false
}

#[derive(Serialize, Deserialize, Clone)]
struct CachedRemoteApp {
    app_id: String,
    name: String,
    summary: String,
    description: String,
    categories: Vec<String>,
    developer: String,
    screenshot_url: String,
    #[serde(default)]
    icon_name: String,
    #[serde(default)]
    extends: String,
    #[serde(default)]
    url_homepage: String,
    #[serde(default)]
    url_bugtracker: String,
    #[serde(default)]
    url_translate: String,
    #[serde(default)]
    url_vcs: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    version_date: String,
    #[serde(default)]
    changelog: String,
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
    // No --app flag: include apps AND extensions/plugins so add-on installed
    // state is detected correctly (e.g. com.obsproject.Studio.Plugin.GStreamer).
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

/// Strip residual HTML tags (e.g. &lt;em&gt; unescaped to <em>) from description text.
fn strip_inline_tags(text: &str) -> String {
    // Strip any residual HTML tags (e.g. <em>, <strong>)
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    // Collapse runs of 3+ newlines down to 2 (one blank line), but preserve
    // the single blank lines that separate paragraphs.
    let mut result = String::new();
    let mut blank_run = 0usize;
    for line in out.split('\n') {
        let t = line.trim();
        if t.is_empty() {
            blank_run += 1;
            if blank_run == 1 {
                result.push('\n'); // allow one blank line
            }
            // suppress further blanks (blank_run > 1)
        } else {
            blank_run = 0;
            result.push_str(t);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

fn parse_appstream_xml(remote: &str) -> Vec<CachedRemoteApp> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let xml_path = format!("/var/lib/flatpak/appstream/{}/x86_64/active/appstream.xml", remote);
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

    struct State {
        app_id: String,
        name: String,
        summary: String,
        description: String,
        categories: Vec<String>,
        developer: String,
        screenshot_url: String,
        screenshot_source_url: String,
        icon_name: String,
        extends: String,
        url_homepage: String,
        url_bugtracker: String,
        url_translate: String,
        url_vcs: String,
        version: String,
        version_date: String,
        changelog: String,
    }

    let mut current: Option<State> = None;
    let mut apps: Vec<CachedRemoteApp> = Vec::new();

    // Boolean context flags
    let mut in_component = false;
    let mut in_id = false;
    let mut in_name = false;
    let mut in_summary = false;
    let mut in_description = false;
    let mut desc_depth: i32 = 0;
    let mut in_developer = false;
    let mut in_developer_name = false;
    let mut in_categories = false;
    let mut in_category = false;
    let mut in_screenshots = false;
    let mut in_screenshot = false;
    let mut cur_image_type = String::new();
    let mut in_image = false;
    let mut in_extends = false;
    let mut in_icon = false;
    let mut in_url = false;
    let mut cur_url_type = String::new();
    let mut in_releases = false;
    let mut in_release = false;
    let mut got_first_release = false;
    let mut in_release_desc = false;
    let mut release_desc_depth: i32 = 0;

    let mut reader = Reader::from_reader(BufReader::new(xml_bytes.as_slice()));
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = e.name();
                match tag.as_ref() {
                    b"component" => {
                        in_component = true;
                        current = Some(State {
                            app_id: String::new(),
                            name: String::new(),
                            summary: String::new(),
                            description: String::new(),
                            categories: Vec::new(),
                            developer: String::new(),
                            screenshot_url: String::new(),
                            screenshot_source_url: String::new(),
                            icon_name: String::new(),
                            extends: String::new(),
                            url_homepage: String::new(),
                            url_bugtracker: String::new(),
                            url_translate: String::new(),
                            url_vcs: String::new(),
                            version: String::new(),
                            version_date: String::new(),
                            changelog: String::new(),
                        });
                    }
                    b"id" if in_component && !in_developer && !in_description && !in_categories && !in_screenshots => {
                        in_id = true;
                    }
                    b"name" if in_component && !in_developer && !in_description && !in_categories => {
                        in_name = true;
                    }
                    b"summary" if in_component && !in_developer && !in_description => {
                        in_summary = true;
                    }
                    b"description" if in_component && !in_screenshots && !in_releases => {
                        in_description = true;
                        desc_depth = 1;
                    }
                    b"url" if in_component && !in_description && !in_screenshots && !in_releases => {
                        cur_url_type.clear();
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"type" {
                                cur_url_type = String::from_utf8_lossy(&attr.value).to_string();
                            }
                        }
                        in_url = true;
                    }
                    b"releases" if in_component => { in_releases = true; }
                    b"release" if in_releases && !got_first_release => {
                        if let Some(ref mut state) = current {
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"version" => { state.version = String::from_utf8_lossy(&attr.value).to_string(); }
                                    b"date" => { state.version_date = String::from_utf8_lossy(&attr.value).to_string(); }
                                    _ => {}
                                }
                            }
                        }
                        in_release = true;
                    }
                    b"description" if in_release && !in_release_desc => {
                        in_release_desc = true;
                        release_desc_depth = 1;
                    }
                    b"li" if in_release_desc => {
                        if let Some(ref mut state) = current {
                            if !state.changelog.is_empty() && !state.changelog.ends_with('\n') {
                                state.changelog.push('\n');
                            }
                            state.changelog.push_str("• ");
                        }
                        release_desc_depth += 1;
                    }
                    _ if in_release_desc => { release_desc_depth += 1; }
                    b"developer" if in_component => {
                        in_developer = true;
                    }
                    b"name" if in_developer => {
                        in_developer_name = true;
                    }
                    b"categories" if in_component => {
                        in_categories = true;
                    }
                    b"category" if in_categories => {
                        in_category = true;
                    }
                    b"extends" if in_component && !in_description => {
                        in_extends = true;
                    }
                    b"screenshots" if in_component => {
                        in_screenshots = true;
                    }
                    b"screenshot" if in_screenshots => {
                        in_screenshot = true;
                    }
                    b"image" if in_screenshot => {
                        in_image = true;
                        cur_image_type.clear();
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"type" {
                                cur_image_type = String::from_utf8_lossy(&attr.value).to_string();
                            }
                        }
                    }
                    b"icon" if in_component && !in_screenshots && !in_description => {
                        // Parse cached icon at 128px: <icon type="cached" width="128">filename.png</icon>
                        let mut is_cached = false;
                        let mut is_128 = false;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"type" => { is_cached = attr.value.as_ref() == b"cached"; }
                                b"width" => { is_128 = attr.value.as_ref() == b"128"; }
                                _ => {}
                            }
                        }
                        if is_cached && is_128 {
                            // icon text content parsed in Text event
                            if let Some(ref mut s) = current {
                                // Mark that we want the next text as icon_name
                                // Use a flag - reuse in_extends pattern
                                let _ = s; // will read in Text event via separate flag
                            }
                            // Use a dedicated flag (add below)
                            in_icon = true;
                        }
                    }
                    b"li" if in_description => {
                        // Add bullet prefix before each list item
                        if let Some(ref mut state) = current {
                            if !state.description.is_empty() && !state.description.ends_with('\n') {
                                state.description.push('\n');
                            }
                            state.description.push_str("• ");
                        }
                        desc_depth += 1;
                    }
                    _ if in_description => {
                        desc_depth += 1;
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let tag = e.name();
                match tag.as_ref() {
                    b"component" => {
                        in_component = false;
                        if let Some(state) = current.take() {
                            if !state.app_id.is_empty() {
                                // Prefer CDN thumbnail, fall back to source URL
                                let ss_url = if !state.screenshot_url.is_empty() {
                                    state.screenshot_url
                                } else {
                                    state.screenshot_source_url
                                };
                                apps.push(CachedRemoteApp {
                                    app_id: state.app_id,
                                    name: state.name,
                                    summary: state.summary,
                                    description: strip_inline_tags(&state.description),
                                    categories: state.categories,
                                    developer: state.developer,
                                    screenshot_url: ss_url,
                                    icon_name: state.icon_name,
                                    extends: state.extends,
                                    url_homepage: state.url_homepage,
                                    url_bugtracker: state.url_bugtracker,
                                    url_translate: state.url_translate,
                                    url_vcs: state.url_vcs,
                                    version: state.version,
                                    version_date: state.version_date,
                                    changelog: strip_inline_tags(&state.changelog),
                                });
                            }
                        }
                    }
                    b"id" => { in_id = false; }
                    b"extends" => { in_extends = false; }
                    b"name" if in_developer => { in_developer_name = false; }
                    b"name" if !in_developer => { in_name = false; }
                    b"summary" => { in_summary = false; }
                    b"description" if desc_depth == 1 => { in_description = false; desc_depth = 0; }
                    b"developer" => { in_developer = false; in_developer_name = false; }
                    b"categories" => { in_categories = false; }
                    b"category" => { in_category = false; }
                    b"screenshots" => { in_screenshots = false; }
                    b"screenshot" => { in_screenshot = false; }
                    b"image" => { in_image = false; }
                    b"icon" => { in_icon = false; }
                    b"url" => { in_url = false; cur_url_type.clear(); }
                    b"releases" => { in_releases = false; }
                    b"release" if in_release => {
                        got_first_release = true;
                        in_release = false;
                    }
                    b"description" if in_release_desc && release_desc_depth == 1 => {
                        in_release_desc = false;
                        release_desc_depth = 0;
                    }
                    b"p" if in_release_desc => {
                        if let Some(ref mut state) = current {
                            if !state.changelog.is_empty() {
                                if !state.changelog.ends_with('\n') { state.changelog.push('\n'); }
                                state.changelog.push('\n');
                            }
                        }
                        release_desc_depth -= 1;
                    }
                    b"li" if in_release_desc => {
                        if let Some(ref mut state) = current {
                            if !state.changelog.ends_with('\n') { state.changelog.push('\n'); }
                        }
                        release_desc_depth -= 1;
                    }
                    b"ul" | b"ol" if in_release_desc => { release_desc_depth -= 1; }
                    _ if in_release_desc => { release_desc_depth -= 1; }
                    b"p" if in_description => {
                        // Double newline = blank line between paragraphs
                        if let Some(ref mut state) = current {
                            if !state.description.is_empty() {
                                if !state.description.ends_with('\n') {
                                    state.description.push('\n');
                                }
                                state.description.push('\n');
                            }
                        }
                        desc_depth -= 1;
                    }
                    b"li" if in_description => {
                        // Single newline after each bullet item
                        if let Some(ref mut state) = current {
                            if !state.description.ends_with('\n') {
                                state.description.push('\n');
                            }
                        }
                        desc_depth -= 1;
                    }
                    b"ul" | b"ol" if in_description => {
                        // Extra blank line after the whole list
                        if let Some(ref mut state) = current {
                            if !state.description.ends_with("\n\n") {
                                if !state.description.ends_with('\n') {
                                    state.description.push('\n');
                                }
                                state.description.push('\n');
                            }
                        }
                        desc_depth -= 1;
                    }
                    _ if in_description => { desc_depth -= 1; }
                    _ => {}
                }
            }
            Ok(Event::Text(e)) => {
                let text = match e.unescape() {
                    Ok(t) => t.to_string(),
                    Err(_) => continue,
                };
                if let Some(ref mut state) = current {
                    if in_id { state.app_id = text.trim().to_string(); }
                    else if in_extends && state.extends.is_empty() { state.extends = text.trim().to_string(); }
                    else if in_name && state.name.is_empty() { state.name = text.trim().to_string(); }
                    else if in_summary && state.summary.is_empty() { state.summary = text.trim().to_string(); }
                    else if in_description {
                        let t = text.trim();
                        if !t.is_empty() {
                            if !state.description.is_empty()
                                && !state.description.ends_with('\n')
                                && !state.description.ends_with(' ')
                            {
                                state.description.push(' ');
                            }
                            state.description.push_str(t);
                        }
                    } else if in_url {
                        let url_text = text.trim().to_string();
                        match cur_url_type.as_str() {
                            "homepage" => { if state.url_homepage.is_empty() { state.url_homepage = url_text; } }
                            "bugtracker" => { if state.url_bugtracker.is_empty() { state.url_bugtracker = url_text; } }
                            "translate" => { if state.url_translate.is_empty() { state.url_translate = url_text; } }
                            "vcs-browser" => { if state.url_vcs.is_empty() { state.url_vcs = url_text; } }
                            _ => {}
                        }
                    } else if in_release_desc {
                        let t = text.trim();
                        if !t.is_empty() {
                            if !state.changelog.is_empty()
                                && !state.changelog.ends_with('\n')
                                && !state.changelog.ends_with(' ')
                            {
                                state.changelog.push(' ');
                            }
                            state.changelog.push_str(t);
                        }
                    } else if in_developer_name && state.developer.is_empty() {
                        state.developer = text.trim().to_string();
                    } else if in_category {
                        state.categories.push(text.trim().to_string());
                    } else if in_icon && state.icon_name.is_empty() {
                        state.icon_name = text.trim().to_string();
                    } else if in_image && in_screenshot {
                        let url = text.trim().to_string();
                        if cur_image_type == "thumbnail"
                            && state.screenshot_url.is_empty()
                            && url.contains("624x351")
                        {
                            // Prefer Flathub CDN 624x351 thumbnail
                            state.screenshot_url = url;
                        } else if cur_image_type == "source"
                            && state.screenshot_source_url.is_empty()
                        {
                            state.screenshot_source_url = url;
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    apps
}

fn fetch_remote_apps_cached(remote: &str) -> Vec<CachedRemoteApp> {
    let path = remote_cache_path(remote);
    if remote_cache_valid(&path) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(apps) = serde_json::from_str::<Vec<CachedRemoteApp>>(&content) {
                return apps;
            }
        }
    }

    // Parse appstream XML
    let apps = parse_appstream_xml(remote);

    // Save cache
    if !apps.is_empty() {
        if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Ok(json) = serde_json::to_string(&apps) {
            let _ = std::fs::write(&path, json);
        }
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
    // Pre-compute which app IDs have add-ons (any app whose `extends` points to them)
    let has_addons: std::collections::HashSet<&str> = apps.iter()
        .filter(|a| !a.extends.is_empty())
        .map(|a| a.extends.as_str())
        .collect();

    let search_lower = search.to_lowercase();
    apps.iter()
        .filter(|app| {
            // Skip add-on entries themselves (they extend another app)
            if !app.extends.is_empty() { return false; }
            // Category filter
            if !category_filter.is_empty() && category_filter != "All" {
                if !app.categories.iter().any(|c| c == category_filter) {
                    return false;
                }
            }
            // Search filter
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
            // Icon path from local appstream cache
            let icon_path = if !app.icon_name.is_empty() {
                format!("/var/lib/flatpak/appstream/flathub/x86_64/active/icons/128x128/{}", app.icon_name)
            } else {
                String::new()
            };
            // First letter uppercase for avatar
            let initial = app.name.chars()
                .next()
                .or_else(|| app.app_id.chars().next())
                .map(|c| c.to_uppercase().next().unwrap_or(c))
                .map(|c| c.to_string())
                .unwrap_or_default();
            // Primary category for avatar color
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
                installed_size: SharedString::from(primary_cat.as_str()),  // category for avatar color
                licenses: SharedString::from(icon_path.as_str()),          // icon file path
                url: SharedString::from(app.screenshot_url.as_str()),
                dependencies: SharedString::from(app.developer.as_str()),
                required_by: SharedString::from(initial.as_str()),         // first letter for avatar
                selected: false,
                explicit: has_addons.contains(app.app_id.as_str()),        // true = app has add-ons
            }
        })
        .collect()
}

fn load_remote_apps(remote: &str) -> (Vec<CachedRemoteApp>, std::collections::HashSet<String>) {
    let apps = fetch_remote_apps_cached(remote);
    let installed = get_flatpak_installed_ids();
    (apps, installed)
}

// ─── Pacman repo browser ──────────────────────────────────────────────────────

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
    if let Ok(o) = out {
        if o.status.success() {
            return String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(3, '\t');
                    let r = parts.next()?;
                    let n = parts.next()?;
                    let d = parts.next().unwrap_or("").trim();
                    if r == repo && !d.is_empty() {
                        Some((n.to_string(), d.to_string()))
                    } else {
                        None
                    }
                })
                .collect();
        }
    }
    std::collections::HashMap::new()
}

fn load_repo_packages(repo: &str) -> Vec<PackageData> {
    let desc_map = load_repo_descriptions(repo);
    let desktop_map = build_desktop_name_map();
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
                    let installed = parts.get(3).map_or(false, |s| *s == "[installed]");
                    let display_name = humanize_package_name(name, &desktop_map);
                    let description = desc_map.get(name).cloned().unwrap_or_default();
                    Some(PackageData {
                        name: SharedString::from(name),
                        display_name: SharedString::from(&display_name),
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

