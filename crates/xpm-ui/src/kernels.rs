//! Kernel Manager data: installed and installable kernel packages, straight from pacman.

use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Kernel {
    pub name: String,
    pub detail: String,
}

pub(crate) fn valid_name(name: &str) -> bool {
    name.starts_with("linux")
        && !name.ends_with("-headers")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

fn is_headers(pkg: &str) -> bool {
    pkg.starts_with("linux") && pkg.ends_with("-headers") && pkg != "linux-api-headers"
}

/// Kernels that ship together with their `-headers` package.
fn pair_with_headers(packages: &[(String, String)]) -> Vec<Kernel> {
    let mut kernels: Vec<Kernel> = packages
        .iter()
        .filter(|(name, _)| is_headers(name))
        .filter_map(|(headers, _)| headers.strip_suffix("-headers"))
        .filter_map(|kernel| packages.iter().find(|(name, _)| name == kernel))
        .map(|(name, detail)| Kernel { name: name.clone(), detail: detail.clone() })
        .collect();
    kernels.sort_by(|a, b| a.name.cmp(&b.name));
    kernels.dedup_by(|a, b| a.name == b.name);
    kernels
}

fn is_testing_repo(repo: &str) -> bool {
    repo == "testing" || repo.ends_with("-testing")
}

/// `pacman -Sl` lines are `repo name version [installed]`.
pub(crate) fn parse_available(sl: &str) -> Vec<Kernel> {
    let packages: Vec<(String, String)> = sl
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let (repo, name, version) = (cols.next()?, cols.next()?, cols.next()?);
            (!is_testing_repo(repo) && name.starts_with("linux")).then(|| (name.to_string(), format!("{repo} · {version}")))
        })
        .collect();
    pair_with_headers(&packages)
}

/// `pacman -Q` lines are `name version`.
pub(crate) fn parse_installed(q: &str) -> Vec<Kernel> {
    let packages: Vec<(String, String)> = q
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let (name, version) = (cols.next()?, cols.next()?);
            name.starts_with("linux").then(|| (name.to_string(), version.to_string()))
        })
        .collect();
    pair_with_headers(&packages)
}

pub(crate) struct Row {
    pub kernel: Kernel,
    pub installed: bool,
}

pub(crate) struct Group {
    pub title: &'static str,
    pub blurb: &'static str,
    pub kind: i32,
    pub rows: Vec<Row>,
}

const FAMILIES: &[(&str, &str)] = &[
    ("Arch Stable", "The standard Arch Linux kernel."),
    ("CachyOS", "Performance tuned kernels with the BORE, EEVDF and sched-ext schedulers."),
    ("LTS", "Long term support kernels that favour stability."),
    ("RT", "Real-time preemption for low latency audio and industrial work."),
    ("Zen", "Desktop focused kernel with responsiveness patches."),
    ("Hardened", "Extra security hardening at some cost in performance."),
    ("XanMod", "General purpose kernel tuned for desktop and gaming."),
    ("Mainline", "Release candidates and upstream builds."),
    ("Other", "Other kernels from your repositories."),
];

fn family(name: &str) -> usize {
    let part = |p: &str| name.split('-').any(|s| s == p);
    if name == "linux" {
        0
    } else if name.starts_with("linux-cachyos") {
        1
    } else if name.contains("xanmod") {
        6
    } else if part("rt") {
        3
    } else if part("lts") {
        2
    } else if name.contains("zen") {
        4
    } else if name.contains("hardened") {
        5
    } else if name.contains("mainline") || part("rc") {
        7
    } else {
        8
    }
}

/// Kernels sorted into families; installed ones first inside each family.
pub(crate) fn group(installed: Vec<Kernel>, available: Vec<Kernel>) -> Vec<Group> {
    let mut groups: Vec<Group> = FAMILIES
        .iter()
        .enumerate()
        .map(|(i, (title, blurb))| Group { title, blurb, kind: i as i32, rows: Vec::new() })
        .collect();
    for (list, is_installed) in [(installed, true), (available, false)] {
        for kernel in list {
            groups[family(&kernel.name)].rows.push(Row { kernel, installed: is_installed });
        }
    }
    groups.retain(|g| !g.rows.is_empty());
    groups
}

fn pacman(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pacman").args(args).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("pacman {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Installed kernels, and kernels in the repositories that are not installed yet.
pub(crate) fn scan() -> (Vec<Kernel>, Vec<Kernel>) {
    let installed = pacman(&["-Q"]).map(|q| parse_installed(&q)).unwrap_or_default();
    let available = pacman(&["-Sl"])
        .map(|sl| parse_available(&sl))
        .unwrap_or_default()
        .into_iter()
        .filter(|k| !installed.iter().any(|i| i.name == k.name))
        .collect();
    (installed, available)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_needs_kernel_and_headers_outside_testing() {
        let sl = "core linux 6.17.3.arch1-1 [installed]\n\
                  core linux-headers 6.17.3.arch1-1 [installed]\n\
                  core linux-api-headers 6.16-1\n\
                  chaotic-aur linux-cachyos 6.17.4-1\n\
                  chaotic-aur linux-cachyos-headers 6.17.4-1\n\
                  chaotic-aur linux-orphan 1-1\n\
                  core-testing linux-zen 6.18-1\n\
                  core-testing linux-zen-headers 6.18-1\n\
                  extra vim 9.1-1\n";
        let names: Vec<_> = parse_available(sl).into_iter().map(|k| k.name).collect();
        assert_eq!(names, ["linux", "linux-cachyos"]);
        assert_eq!(parse_available(sl)[1].detail, "chaotic-aur · 6.17.4-1");
    }

    #[test]
    fn kernels_are_sorted_into_families() {
        let k = |name: &str| Kernel { name: name.into(), detail: String::new() };
        let groups = group(
            vec![k("linux-cachyos"), k("linux")],
            vec![k("linux-lts"), k("linux-cachyos-lts"), k("linux-rt-lts"), k("linux-zen"), k("linux-hardened"), k("linux-xanmod"), k("linux-weird")],
        );
        let layout: Vec<(&str, Vec<&str>)> =
            groups.iter().map(|g| (g.title, g.rows.iter().map(|r| r.kernel.name.as_str()).collect())).collect();
        assert_eq!(
            layout,
            [
                ("Arch Stable", vec!["linux"]),
                ("CachyOS", vec!["linux-cachyos", "linux-cachyos-lts"]),
                ("LTS", vec!["linux-lts"]),
                ("RT", vec!["linux-rt-lts"]),
                ("Zen", vec!["linux-zen"]),
                ("Hardened", vec!["linux-hardened"]),
                ("XanMod", vec!["linux-xanmod"]),
                ("Other", vec!["linux-weird"]),
            ]
        );
        assert!(groups[1].rows[0].installed && !groups[1].rows[1].installed);
    }

    #[test]
    fn installed_kernels_need_their_headers() {
        let q = "bash 5.3-1\nlinux 6.17.3.arch1-1\nlinux-headers 6.17.3.arch1-1\nlinux-firmware 2025-1\nlinux-lts 6.12-1\n";
        let kernels = parse_installed(q);
        assert_eq!(kernels, [Kernel { name: "linux".into(), detail: "6.17.3.arch1-1".into() }]);
    }
}
