//! Parse /var/log/pacman.log into transactions and locate old package versions
//! for rollback. Pure + unit-tested; fs/network access lives in the UI layer.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Installed,
    Upgraded,
    Downgraded,
    Reinstalled,
    Removed,
}

impl ActionKind {
    pub fn label(self) -> &'static str {
        match self {
            ActionKind::Installed => "installed",
            ActionKind::Upgraded => "upgraded",
            ActionKind::Downgraded => "downgraded",
            ActionKind::Reinstalled => "reinstalled",
            ActionKind::Removed => "removed",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogAction {
    pub kind: ActionKind,
    pub pkg: String,
    /// Prior version (upgraded/downgraded/removed); empty for installs.
    pub old: String,
    /// New version (installed/upgraded/downgraded/reinstalled); empty for removes.
    pub new: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Transaction {
    pub when: String,
    pub command: String,
    pub actions: Vec<LogAction>,
}

impl Transaction {
    pub fn count(&self, kind: ActionKind) -> usize {
        self.actions.iter().filter(|a| a.kind == kind).count()
    }
    /// Packages this transaction upgraded, as (name, previous_version).
    pub fn upgraded_targets(&self) -> Vec<(String, String)> {
        self.actions
            .iter()
            .filter(|a| a.kind == ActionKind::Upgraded && !a.old.is_empty())
            .map(|a| (a.pkg.clone(), a.old.clone()))
            .collect()
    }
}

/// Split a log line into (timestamp, source, message), e.g.
/// `[2026-06-25T10:00:00+0000] [ALPM] upgraded foo (1-1 -> 1-2)`.
fn split_line(line: &str) -> Option<(String, String, String)> {
    let line = line.trim();
    if !line.starts_with('[') {
        return None;
    }
    let ts_end = line.find(']')?;
    let ts = line[1..ts_end].to_string();
    let rest = line[ts_end + 1..].trim_start();
    if !rest.starts_with('[') {
        return None;
    }
    let src_end = rest.find(']')?;
    let src = rest[1..src_end].to_string();
    let msg = rest[src_end + 1..].trim_start().to_string();
    Some((ts, src, msg))
}

/// Parse an ALPM action message like `upgraded foo (1-1 -> 1-2)`.
fn parse_action(msg: &str) -> Option<LogAction> {
    let (verb, tail) = msg.split_once(' ')?;
    let kind = match verb {
        "installed" => ActionKind::Installed,
        "upgraded" => ActionKind::Upgraded,
        "downgraded" => ActionKind::Downgraded,
        "reinstalled" => ActionKind::Reinstalled,
        "removed" => ActionKind::Removed,
        _ => return None,
    };
    let paren = tail.find('(')?;
    let pkg = tail[..paren].trim().to_string();
    let inside = tail[paren + 1..].trim_end().trim_end_matches(')').trim().to_string();
    let (old, new) = match inside.split_once(" -> ") {
        Some((o, n)) => (o.trim().to_string(), n.trim().to_string()),
        None => match kind {
            ActionKind::Removed => (inside.clone(), String::new()),
            _ => (String::new(), inside.clone()),
        },
    };
    if pkg.is_empty() {
        return None;
    }
    Some(LogAction { kind, pkg, old, new })
}

/// Parse a full pacman.log into transactions, newest first.
pub fn parse_log(text: &str) -> Vec<Transaction> {
    let mut txns: Vec<Transaction> = Vec::new();
    let mut last_command = String::new();
    let mut cur: Option<Transaction> = None;

    for line in text.lines() {
        let Some((ts, src, msg)) = split_line(line) else { continue };
        if src == "PACMAN" {
            if let Some(rest) = msg.strip_prefix("Running ") {
                last_command = rest.trim().trim_matches('\'').trim_matches('"').to_string();
            }
            continue;
        }
        if src != "ALPM" {
            continue;
        }
        if msg == "transaction started" {
            cur = Some(Transaction { when: ts, command: last_command.clone(), actions: Vec::new() });
        } else if msg == "transaction completed" {
            if let Some(t) = cur.take()
                && !t.actions.is_empty() {
                    txns.push(t);
                }
        } else if let Some(t) = cur.as_mut()
            && let Some(a) = parse_action(&msg) {
                t.actions.push(a);
            }
    }
    // Flush a transaction missing its "completed" line (interrupted run).
    if let Some(t) = cur
        && !t.actions.is_empty() {
            txns.push(t);
        }
    txns.reverse(); // newest first
    txns
}

/// Cache filename for an exact version, given a package arch (`x86_64` / `any`).
pub fn cache_filename(name: &str, ver: &str, arch: &str) -> String {
    format!("{name}-{ver}-{arch}.pkg.tar.zst")
}

/// Arch Linux Archive URL for an exact package version.
pub fn ala_url(name: &str, ver: &str, arch: &str) -> String {
    let first = name.chars().next().unwrap_or('-').to_ascii_lowercase();
    format!("https://archive.archlinux.org/packages/{first}/{name}/{name}-{ver}-{arch}.pkg.tar.zst")
}

/// Packages widely considered risky to downgrade (boot/login critical).
pub fn is_critical(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "glibc", "systemd", "systemd-libs", "pacman", "linux", "linux-lts",
        "linux-zen", "linux-hardened", "mkinitcpio", "grub", "systemd-boot",
        "limine", "mesa", "gcc-libs",
    ];
    EXACT.contains(&name) || name.starts_with("linux") || name.starts_with("nvidia")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "\
[2026-06-24T09:00:00+0000] [PACMAN] Running 'pacman -S gimp'
[2026-06-24T09:00:01+0000] [ALPM] transaction started
[2026-06-24T09:00:02+0000] [ALPM] installed gimp (2.10.36-1)
[2026-06-24T09:00:03+0000] [ALPM] transaction completed
[2026-06-25T10:00:00+0000] [PACMAN] Running 'pacman -Syu'
[2026-06-25T10:00:01+0000] [ALPM] transaction started
[2026-06-25T10:00:02+0000] [ALPM] upgraded bar (1.0-1 -> 1.1-1)
[2026-06-25T10:00:03+0000] [ALPM] upgraded linux (6.9-1 -> 6.10-1)
[2026-06-25T10:00:04+0000] [ALPM] removed oldpkg (3.0-1)
[2026-06-25T10:00:05+0000] [ALPM] transaction completed
";

    #[test]
    fn parses_two_transactions_newest_first() {
        let t = parse_log(LOG);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].command, "pacman -Syu");
        assert_eq!(t[1].command, "pacman -S gimp");
    }

    #[test]
    fn parses_action_kinds_and_versions() {
        let t = parse_log(LOG);
        let up = &t[0];
        assert_eq!(up.count(ActionKind::Upgraded), 2);
        assert_eq!(up.count(ActionKind::Removed), 1);
        let bar = up.actions.iter().find(|a| a.pkg == "bar").unwrap();
        assert_eq!(bar.kind, ActionKind::Upgraded);
        assert_eq!(bar.old, "1.0-1");
        assert_eq!(bar.new, "1.1-1");
        let gimp = &t[1].actions[0];
        assert_eq!(gimp.kind, ActionKind::Installed);
        assert_eq!(gimp.new, "2.10.36-1");
        assert!(gimp.old.is_empty());
    }

    #[test]
    fn upgraded_targets_are_old_versions() {
        let t = parse_log(LOG);
        let targets = t[0].upgraded_targets();
        assert!(targets.contains(&("bar".to_string(), "1.0-1".to_string())));
        assert!(targets.contains(&("linux".to_string(), "6.9-1".to_string())));
    }

    #[test]
    fn builds_cache_name_and_ala_url() {
        assert_eq!(cache_filename("bar", "1.0-1", "x86_64"), "bar-1.0-1-x86_64.pkg.tar.zst");
        assert_eq!(
            ala_url("gimp", "2.10-1", "x86_64"),
            "https://archive.archlinux.org/packages/g/gimp/gimp-2.10-1-x86_64.pkg.tar.zst"
        );
    }

    #[test]
    fn flags_critical_packages() {
        assert!(is_critical("glibc"));
        assert!(is_critical("linux-zen"));
        assert!(is_critical("nvidia-utils"));
        assert!(!is_critical("gimp"));
    }
}
