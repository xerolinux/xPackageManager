//! Flatpak permission overrides. Parses `flatpak info` keyfiles (metadata =
//! defaults, --show-permissions = effective) and builds `flatpak override`
//! argument vectors. Pure + unit-tested; the actual flatpak/pkexec invocations
//! live in the UI layer.

use std::collections::BTreeMap;

/// Parsed `[section] -> key -> value` keyfile.
pub type KeyFile = BTreeMap<String, BTreeMap<String, String>>;

pub const CTX: &str = "Context";
pub const SESSION_BUS: &str = "Session Bus Policy";
pub const SYSTEM_BUS: &str = "System Bus Policy";
pub const ENVIRONMENT: &str = "Environment";

/// Parse a flatpak metadata / permissions keyfile into sections.
pub fn parse_keyfile(text: &str) -> KeyFile {
    let mut out: KeyFile = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if l.starts_with('[') && l.ends_with(']') {
            section = l[1..l.len() - 1].to_string();
            out.entry(section.clone()).or_default();
        } else if let Some(eq) = l.find('=') {
            let k = l[..eq].trim().to_string();
            let v = l[eq + 1..].trim().to_string();
            out.entry(section.clone()).or_default().insert(k, v);
        }
    }
    out
}

/// Tokens of a `;`-separated Context list value (empties dropped).
pub fn list_tokens(kf: &KeyFile, key: &str) -> Vec<String> {
    kf.get(CTX)
        .and_then(|s| s.get(key))
        .map(|v| {
            v.split(';')
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .map(|t| t.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub fn has_token(kf: &KeyFile, key: &str, token: &str) -> bool {
    list_tokens(kf, key).iter().any(|t| t == token)
}

/// All `name -> policy` entries of a bus-policy section.
pub fn bus_entries(kf: &KeyFile, section: &str) -> BTreeMap<String, String> {
    kf.get(section).cloned().unwrap_or_default()
}

/// All `KEY -> value` environment entries.
pub fn env_entries(kf: &KeyFile) -> BTreeMap<String, String> {
    kf.get(ENVIRONMENT).cloned().unwrap_or_default()
}

/// The Context list key a togglable permission category writes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Shared,
    Socket,
    Device,
    Feature,
    Filesystem,
}

impl Category {
    /// The `[Context]` key this category is stored under.
    pub fn ctx_key(self) -> &'static str {
        match self {
            Category::Shared => "shared",
            Category::Socket => "sockets",
            Category::Device => "devices",
            Category::Feature => "features",
            Category::Filesystem => "filesystems",
        }
    }
}

/// One `flatpak override` flag toggling a Context list token on/off. For
/// filesystems an optional `mode` (rw/ro/create) is appended when granting.
pub fn toggle_arg(cat: Category, token: &str, on: bool, mode: Option<&str>) -> String {
    match (cat, on) {
        (Category::Shared, true) => format!("--share={token}"),
        (Category::Shared, false) => format!("--unshare={token}"),
        (Category::Socket, true) => format!("--socket={token}"),
        (Category::Socket, false) => format!("--nosocket={token}"),
        (Category::Device, true) => format!("--device={token}"),
        (Category::Device, false) => format!("--nodevice={token}"),
        (Category::Feature, true) => format!("--allow={token}"),
        (Category::Feature, false) => format!("--disallow={token}"),
        (Category::Filesystem, true) => match mode {
            Some(m) if !m.is_empty() => format!("--filesystem={token}:{m}"),
            _ => format!("--filesystem={token}"),
        },
        (Category::Filesystem, false) => format!("--nofilesystem={token}"),
    }
}

/// Flag to grant/revoke a session- or system-bus name (talk policy).
pub fn bus_arg(system: bool, name: &str, talk: bool) -> String {
    match (system, talk) {
        (false, true) => format!("--talk-name={name}"),
        (false, false) => format!("--no-talk-name={name}"),
        (true, true) => format!("--system-talk-name={name}"),
        (true, false) => format!("--system-no-talk-name={name}"),
    }
}

/// Flag to set or unset an environment variable.
pub fn env_arg(key: &str, value: Option<&str>) -> String {
    match value {
        Some(v) => format!("--env={key}={v}"),
        None => format!("--unset-env={key}"),
    }
}

/// Flag to add a persisted relative path.
pub fn persist_arg(path: &str) -> String {
    format!("--persist={path}")
}

/// Build a full `flatpak override` argv (without the leading program) for one or
/// more flags against an app. `scope_system` selects --system vs --user.
pub fn override_argv(scope_system: bool, app_id: &str, flags: &[String]) -> Vec<String> {
    let mut v = vec!["override".to_string()];
    v.push(if scope_system { "--system".into() } else { "--user".into() });
    v.extend(flags.iter().cloned());
    v.push(app_id.to_string());
    v
}

/// Build the `flatpak override --reset` argv for an app.
pub fn reset_argv(scope_system: bool, app_id: &str) -> Vec<String> {
    vec![
        "override".to_string(),
        if scope_system { "--system".into() } else { "--user".into() },
        "--reset".into(),
        app_id.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = "\
[Application]
name=org.example.App

[Context]
shared=network;ipc;
sockets=x11;wayland;pulseaudio;
devices=dri;
filesystems=host;xdg-download;

[Session Bus Policy]
org.freedesktop.Notifications=talk

[Environment]
LANG=C
";

    const EFFECTIVE: &str = "\
[Context]
shared=ipc;
sockets=wayland;pulseaudio;
devices=dri;
filesystems=xdg-download;~/Games;

[Session Bus Policy]
org.freedesktop.Notifications=talk
org.example.Extra=talk

[Environment]
LANG=C
EDITOR=vim
";

    #[test]
    fn parses_sections_and_lists() {
        let kf = parse_keyfile(META);
        assert!(has_token(&kf, "shared", "network"));
        assert!(has_token(&kf, "sockets", "x11"));
        assert!(has_token(&kf, "sockets", "wayland")); // wayland present
        assert_eq!(list_tokens(&kf, "filesystems"), vec!["host", "xdg-download"]);
        assert_eq!(env_entries(&kf).get("LANG").map(|s| s.as_str()), Some("C"));
        assert_eq!(
            bus_entries(&kf, SESSION_BUS).get("org.freedesktop.Notifications").map(|s| s.as_str()),
            Some("talk")
        );
    }

    #[test]
    fn defaults_vs_effective_overrides() {
        let def = parse_keyfile(META);
        let eff = parse_keyfile(EFFECTIVE);
        // network was a default, removed by override -> off in effective.
        assert!(has_token(&def, "shared", "network"));
        assert!(!has_token(&eff, "shared", "network"));
        // x11 default removed; wayland kept.
        assert!(has_token(&def, "sockets", "x11"));
        assert!(!has_token(&eff, "sockets", "x11"));
        assert!(has_token(&eff, "sockets", "wayland"));
        // host removed, custom ~/Games added.
        assert!(has_token(&def, "filesystems", "host"));
        assert!(!has_token(&eff, "filesystems", "host"));
        assert!(has_token(&eff, "filesystems", "~/Games"));
        // extra bus name + env var added by override.
        assert!(bus_entries(&eff, SESSION_BUS).contains_key("org.example.Extra"));
        assert_eq!(env_entries(&eff).get("EDITOR").map(|s| s.as_str()), Some("vim"));
    }

    #[test]
    fn builds_toggle_flags() {
        assert_eq!(toggle_arg(Category::Shared, "network", false, None), "--unshare=network");
        assert_eq!(toggle_arg(Category::Shared, "network", true, None), "--share=network");
        assert_eq!(toggle_arg(Category::Socket, "x11", false, None), "--nosocket=x11");
        assert_eq!(toggle_arg(Category::Device, "dri", true, None), "--device=dri");
        assert_eq!(toggle_arg(Category::Feature, "devel", false, None), "--disallow=devel");
        assert_eq!(toggle_arg(Category::Filesystem, "host", true, None), "--filesystem=host");
        assert_eq!(toggle_arg(Category::Filesystem, "/data", true, Some("ro")), "--filesystem=/data:ro");
        assert_eq!(toggle_arg(Category::Filesystem, "host", false, None), "--nofilesystem=host");
    }

    #[test]
    fn builds_bus_env_persist_flags() {
        assert_eq!(bus_arg(false, "org.x.Y", true), "--talk-name=org.x.Y");
        assert_eq!(bus_arg(false, "org.x.Y", false), "--no-talk-name=org.x.Y");
        assert_eq!(bus_arg(true, "org.x.Y", true), "--system-talk-name=org.x.Y");
        assert_eq!(env_arg("EDITOR", Some("vim")), "--env=EDITOR=vim");
        assert_eq!(env_arg("EDITOR", None), "--unset-env=EDITOR");
        assert_eq!(persist_arg(".mozilla"), "--persist=.mozilla");
    }

    #[test]
    fn builds_override_and_reset_argv() {
        let flags = vec!["--unshare=network".to_string(), "--socket=wayland".to_string()];
        assert_eq!(
            override_argv(false, "org.example.App", &flags),
            vec!["override", "--user", "--unshare=network", "--socket=wayland", "org.example.App"]
        );
        assert_eq!(
            reset_argv(true, "org.example.App"),
            vec!["override", "--system", "--reset", "org.example.App"]
        );
    }
}
