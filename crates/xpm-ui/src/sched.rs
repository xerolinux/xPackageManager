//! Kernel Schedulers page logic: sched-ext availability, `scxctl` parsing and the boot service.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) const MODES: [(&str, &str); 4] =
    [("Auto", "auto"), ("Gaming", "gaming"), ("Low Latency", "lowlatency"), ("Power Save", "powersave")];

const CATEGORIES: &[(&str, &[&str])] = &[
    ("Gaming", &["scx_rusty", "scx_lavd", "scx_bpfland"]),
    ("Desktop", &["scx_cosmos", "scx_flash"]),
    ("Servers", &["scx_layered", "scx_flatcg", "scx_tickless"]),
    ("Low Latency", &["scx_nest"]),
    ("Testing", &["scx_simple", "scx_chaos", "scx_userland"]),
];

const SERVICE_PATH: &str = "/etc/systemd/system/scx.service";

pub(crate) struct KernelSupport {
    pub version: String,
    /// 0 supported, 1 maybe, 2 unsupported.
    pub level: i32,
    pub message: String,
}

pub(crate) fn kernel_support() -> KernelSupport {
    let version = Command::new("uname")
        .arg("-r")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let (level, message) = assess_kernel(&version, Path::new("/sys/kernel/sched_ext").exists());
    KernelSupport { version, level, message: message.to_string() }
}

fn assess_kernel(version: &str, sched_ext_present: bool) -> (i32, &'static str) {
    if sched_ext_present {
        return (0, "Sched-ext is supported");
    }
    let mut parts = version.split('.');
    let major = parts.next().and_then(|p| p.parse::<u32>().ok());
    let minor = parts.next().and_then(|p| p.split('-').next()).and_then(|p| p.parse::<u32>().ok());
    match (major, minor) {
        (Some(major), Some(minor)) if major > 6 || (major == 6 && minor >= 12) => (1, "Kernel 6.12+ but sched-ext not detected"),
        (Some(_), Some(_)) => (2, "Requires kernel 6.12+"),
        _ => (1, "Could not parse version"),
    }
}

pub(crate) fn humanize_name(scheduler: &str) -> String {
    if scheduler.is_empty() || scheduler == "EEVDF" {
        return scheduler.to_string();
    }
    let name = scheduler.trim_start_matches("scx_");
    match name {
        "bpfland" => "BPFland".to_string(),
        "lavd" => "LAVD".to_string(),
        "p2dq" => "P2DQ".to_string(),
        _ => {
            let mut chars = name.chars();
            chars.next().map(|c| c.to_uppercase().chain(chars.flat_map(|c| c.to_lowercase())).collect()).unwrap_or_default()
        }
    }
}

pub(crate) fn humanize_mode(mode: &str) -> String {
    match mode.to_lowercase().as_str() {
        "auto" => "Auto".to_string(),
        "gaming" => "Gaming".to_string(),
        "lowlatency" => "Low Latency".to_string(),
        "powersave" => "Power Save".to_string(),
        _ => mode.to_string(),
    }
}

/// `supported schedulers: ["bpfland", "lavd"]` becomes `scx_bpfland`, `scx_lavd`.
pub(crate) fn parse_list(out: &str) -> Vec<String> {
    let Some((_, rest)) = out.split_once("supported schedulers:") else { return Vec::new() };
    let (Some(start), Some(end)) = (rest.find('['), rest.find(']')) else { return Vec::new() };
    if end < start {
        return Vec::new();
    }
    rest[start + 1..end]
        .split(',')
        .map(|s| s.trim().trim_matches('"'))
        .filter(|s| !s.is_empty())
        .map(|s| format!("scx_{s}"))
        .collect()
}

/// Known categories first (in the original order), then everything else sorted.
pub(crate) fn group(available: &[String]) -> Vec<(String, Vec<String>)> {
    let mut groups: Vec<(String, Vec<String>)> = CATEGORIES
        .iter()
        .map(|(title, members)| {
            (title.to_string(), members.iter().filter(|m| available.iter().any(|a| a == *m)).map(|m| m.to_string()).collect())
        })
        .filter(|(_, items): &(String, Vec<String>)| !items.is_empty())
        .collect();
    let mut other: Vec<String> = available
        .iter()
        .filter(|a| !CATEGORIES.iter().any(|(_, members)| members.contains(&a.as_str())))
        .cloned()
        .collect();
    other.sort();
    if !other.is_empty() {
        groups.push(("Other".to_string(), other));
    }
    groups
}

#[derive(Debug, PartialEq, Eq, Default, Clone)]
pub(crate) struct Status {
    pub active: bool,
    pub name: String,
    pub mode: String,
}

/// `running bpfland in gaming mode` or `no scx scheduler running`.
pub(crate) fn parse_status(out: &str) -> Status {
    let out = out.trim();
    let lower = out.to_lowercase();
    if out.is_empty() || lower.contains("not running") || !lower.starts_with("running") {
        return Status::default();
    }
    let Some(name) = out.split_whitespace().nth(1) else { return Status::default() };
    let mode = out
        .split_once(" in ")
        .and_then(|(_, tail)| tail.split(" mode").next())
        .map(|m| m.trim().to_string())
        .unwrap_or_else(|| "N/A".to_string());
    Status { active: true, name: format!("scx_{}", name.to_lowercase()), mode }
}

pub(crate) fn scxctl_available() -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join("scxctl").is_file()))
        .unwrap_or(false)
}

/// Runs `scxctl`, giving up (and killing it) after `timeout` so a stuck D-Bus call cannot hang the page.
fn scxctl_within(args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new("scxctl")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let started = Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(_) => break,
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("scxctl {} timed out", args.join(" ")));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub(crate) fn scxctl(args: &[&str]) -> Result<String, String> {
    scxctl_within(args, Duration::from_secs(10))
}

pub(crate) fn status() -> Status {
    scxctl_within(&["get"], Duration::from_secs(2)).map(|out| parse_status(&out)).unwrap_or_default()
}

pub(crate) fn schedulers() -> Result<Vec<String>, String> {
    scxctl_within(&["list"], Duration::from_secs(5)).map(|out| parse_list(&out))
}

pub(crate) fn persistence_enabled() -> bool {
    Command::new("systemctl")
        .args(["is-enabled", "scx.service"])
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("enabled"))
        .unwrap_or(false)
}

fn valid_scheduler(name: &str) -> bool {
    name.starts_with("scx_") && name.len() > 4 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn service_unit(scheduler: &str, mode: &str) -> Result<String, String> {
    if !valid_scheduler(scheduler) || !MODES.iter().any(|(_, m)| *m == mode) {
        return Err("Unsupported scheduler or mode".to_string());
    }
    let name = scheduler.trim_start_matches("scx_");
    Ok(format!(
        "[Unit]\n\
         Description=sched-ext BPF CPU Scheduler ({scheduler})\n\
         After=sysinit.target local-fs.target\n\
         DefaultDependencies=no\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         RemainAfterExit=yes\n\
         ExecStart=/bin/sh -c '/usr/bin/scxctl stop 2>/dev/null || true; /usr/bin/scxctl start --sched {name} --mode {mode}'\n\
         ExecStop=/usr/bin/scxctl stop\n\
         TimeoutStartSec=30\n\
         \n\
         [Install]\n\
         WantedBy=sysinit.target\n"
    ))
}

fn pkexec_sh(script: &str, stdin: Option<&str>) -> Result<(), String> {
    let mut child = Command::new("pkexec")
        .args(["sh", "-c", script])
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

pub(crate) fn enable_persistence(scheduler: &str, mode: &str) -> Result<(), String> {
    let unit = service_unit(scheduler, mode)?;
    pkexec_sh(
        &format!("cat > {SERVICE_PATH} && systemctl daemon-reload && systemctl enable --now scx.service"),
        Some(&unit),
    )
}

pub(crate) fn disable_persistence() -> Result<(), String> {
    pkexec_sh("systemctl stop scx.service && systemctl disable scx.service", None)
}

pub(crate) fn service_active() -> bool {
    Command::new("systemctl").args(["is-active", "scx.service"]).status().map(|s| s.success()).unwrap_or(false)
}

pub(crate) struct Snapshot {
    pub missing: bool,
    pub support: KernelSupport,
    pub groups: Vec<(String, Vec<String>)>,
    pub status: Status,
    pub persist: bool,
    pub list_error: Option<String>,
}

pub(crate) fn snapshot() -> Snapshot {
    let missing = !scxctl_available();
    let listed = if missing { Ok(Vec::new()) } else { schedulers() };
    Snapshot {
        missing,
        support: kernel_support(),
        list_error: listed.as_ref().err().cloned(),
        groups: group(listed.as_deref().unwrap_or_default()),
        status: if missing { Status::default() } else { status() },
        persist: persistence_enabled(),
    }
}

pub(crate) fn valid_request(scheduler: &str, mode: &str) -> bool {
    valid_scheduler(scheduler) && MODES.iter().any(|(_, m)| *m == mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_list_is_prefixed() {
        let out = "supported schedulers: [\"beerland\", \"bpfland\", \"lavd\"]";
        assert_eq!(parse_list(out), ["scx_beerland", "scx_bpfland", "scx_lavd"]);
        assert!(parse_list("error").is_empty());
    }

    #[test]
    fn status_lines() {
        assert_eq!(parse_status("no scx scheduler running"), Status::default());
        assert_eq!(parse_status(""), Status::default());
        assert_eq!(
            parse_status("running bpfland in gaming mode"),
            Status { active: true, name: "scx_bpfland".into(), mode: "gaming".into() }
        );
        assert_eq!(parse_status("running Lavd").mode, "N/A");
    }

    #[test]
    fn names_and_modes_read_well() {
        assert_eq!(humanize_name("scx_bpfland"), "BPFland");
        assert_eq!(humanize_name("scx_lavd"), "LAVD");
        assert_eq!(humanize_name("scx_tickless"), "Tickless");
        assert_eq!(humanize_name("EEVDF"), "EEVDF");
        assert_eq!(humanize_mode("lowlatency"), "Low Latency");
    }

    #[test]
    fn schedulers_are_grouped_with_leftovers_last() {
        let available: Vec<String> = ["scx_cake", "scx_lavd", "scx_rusty", "scx_flash", "scx_beerland"].iter().map(|s| s.to_string()).collect();
        let groups = group(&available);
        assert_eq!(groups[0], ("Gaming".to_string(), vec!["scx_rusty".to_string(), "scx_lavd".to_string()]));
        assert_eq!(groups[1].0, "Desktop");
        assert_eq!(groups[2], ("Other".to_string(), vec!["scx_beerland".to_string(), "scx_cake".to_string()]));
    }

    #[test]
    fn kernel_support_follows_the_original_rules() {
        assert_eq!(assess_kernel("6.1.0", true).0, 0);
        assert_eq!(assess_kernel("6.17.3-arch1", false).0, 1);
        assert_eq!(assess_kernel("6.6.2-lts", false).0, 2);
        assert_eq!(assess_kernel("weird", false).0, 1);
    }

    #[test]
    fn unit_rejects_unknown_input() {
        assert!(service_unit("scx_lavd", "gaming").unwrap().contains("scxctl start --sched lavd --mode gaming"));
        assert!(service_unit("scx_lavd; rm -rf /", "gaming").is_err());
        assert!(service_unit("scx_lavd", "x'; reboot").is_err());
    }
}
