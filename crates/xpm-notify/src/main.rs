use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use std::thread;

const DEFAULT_INTERVAL_MINS: u64 = 30;
const INITIAL_DELAY_SECS: u64 = 60; // wait 1 min after login before first check

fn config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(format!("{}/.config/xpm/config.json", home))
}

fn load_interval_minutes() -> u64 {
    let path = config_path();
    if let Ok(content) = std::fs::read_to_string(&path) {
        // Simple key extraction — no serde dep needed
        if let Some(pos) = content.find("\"notify_interval_minutes\"") {
            let after = &content[pos + "\"notify_interval_minutes\"".len()..];
            if let Some(colon) = after.find(':') {
                let val_str = after[colon + 1..].trim();
                let num: String = val_str.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = num.parse::<u64>() {
                    if n > 0 {
                        return n;
                    }
                }
            }
        }
    }
    DEFAULT_INTERVAL_MINS
}

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(format!("{}/.local/share/xpm", home))
}

fn pidfile_path() -> PathBuf {
    data_dir().join("notify.pid")
}


fn is_already_running(pidfile: &Path) -> bool {
    if let Ok(content) = std::fs::read_to_string(pidfile) {
        if let Ok(pid) = content.trim().parse::<u32>() {
            // On Linux, /proc/<pid> exists only if the process is alive
            if Path::new(&format!("/proc/{}", pid)).exists() {
                return true;
            }
        }
    }
    false
}

fn write_pidfile(pidfile: &Path) {
    if let Some(parent) = pidfile.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(pidfile, std::process::id().to_string());
}

fn remove_pidfile(pidfile: &Path) {
    let _ = std::fs::remove_file(pidfile);
}

fn check_updates() -> Vec<String> {
    let output = Command::new("checkupdates").output().unwrap_or_else(|_| {
        // fallback: pacman -Qu (requires sudo for db sync, skip)
        std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: vec![],
            stderr: vec![],
        }
    });

    // checkupdates exits 0 with updates, 2 with no updates, 1 on error
    if output.stdout.is_empty() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect()
}

fn check_flatpak_updates() -> usize {
    let output = Command::new("flatpak")
        .args(["remote-ls", "--updates", "--app"])
        .output();

    match output {
        Ok(o) if !o.stdout.is_empty() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        }
        _ => 0,
    }
}

fn send_notification(pacman_count: usize, flatpak_count: usize) {
    let total = pacman_count + flatpak_count;
    if total == 0 {
        return;
    }

    let summary = format!("{} update{} available", total, if total == 1 { "" } else { "s" });

    let mut body_parts = Vec::new();
    if pacman_count > 0 {
        body_parts.push(format!("{} pacman package{}", pacman_count, if pacman_count == 1 { "" } else { "s" }));
    }
    if flatpak_count > 0 {
        body_parts.push(format!("{} flatpak app{}", flatpak_count, if flatpak_count == 1 { "" } else { "s" }));
    }
    let body = body_parts.join(", ");

    let _ = Command::new("notify-send")
        .args([
            "--app-name=xPackageManager",
            "--icon=system-software-update",
            "--urgency=normal",
            "--expire-time=8000",
            &summary,
            &body,
        ])
        .spawn();
}

fn open_xpackagemanager() {
    // Try to find the binary next to this daemon
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    if let Some(dir) = exe_dir {
        let xpm = dir.join("xpackagemanager");
        if xpm.exists() {
            let _ = Command::new(&xpm).spawn();
            return;
        }
    }

    // Fallback: search PATH
    let _ = Command::new("xpackagemanager").spawn();
}

fn main() {
    let pidfile = pidfile_path();

    if is_already_running(&pidfile) {
        eprintln!("xpm-notify: already running ({})", pidfile.display());
        std::process::exit(0);
    }

    write_pidfile(&pidfile);

    // Register cleanup on SIGTERM/SIGINT via atexit-style approach
    // We re-check the pidfile path in a drop guard
    let pidfile_clone = pidfile.clone();
    let _guard = PidGuard(pidfile_clone);

    eprintln!("xpm-notify: started (PID {})", std::process::id());

    // Initial delay — give the system time to settle after login
    thread::sleep(Duration::from_secs(INITIAL_DELAY_SECS));

    loop {
        eprintln!("xpm-notify: checking for updates...");

        let pacman_updates = check_updates();
        let flatpak_count = check_flatpak_updates();
        let pacman_count = pacman_updates.len();

        if pacman_count > 0 || flatpak_count > 0 {
            eprintln!(
                "xpm-notify: found {} pacman + {} flatpak updates",
                pacman_count, flatpak_count
            );
            send_notification(pacman_count, flatpak_count);
        } else {
            eprintln!("xpm-notify: system is up to date");
        }

        let interval = load_interval_minutes() * 60;
        thread::sleep(Duration::from_secs(interval));
    }
}

struct PidGuard(PathBuf);

impl Drop for PidGuard {
    fn drop(&mut self) {
        remove_pidfile(&self.0);
    }
}
