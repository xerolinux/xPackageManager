//! Decides whether an update leaves the system needing a reboot.
//!
//! Arch has no "reboot required" flag, so two signals are combined:
//! - the packages about to be updated include something loaded at boot: the running kernel,
//!   firmware, microcode, initramfs tooling, kernel modules (dkms), mesa and the video drivers,
//!   systemd, dbus or glibc;
//! - the system is already out of date: the running kernel's modules are gone, boot-bound
//!   packages were installed after boot, or the loaded NVIDIA module differs from its userspace.

use std::fs;
use std::path::Path;

const PACMAN_LOCAL: &str = "/var/lib/pacman/local";
const MODULES: &str = "/usr/lib/modules";

pub fn needed(updated: &[String]) -> bool {
    let release = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let release = release.trim();
    let kernel = running_kernel_package(Path::new(MODULES), release);
    if updated.iter().any(|n| boot_bound(n) || kernel.as_deref() == Some(n.as_str())) {
        return true;
    }
    let boot = fs::read_to_string("/proc/stat").ok().and_then(|s| boot_time(&s));
    let loaded_nvidia = fs::read_to_string("/sys/module/nvidia/version").ok();
    let installed = installed_packages(Path::new(PACMAN_LOCAL));
    pending(release, Path::new(MODULES), boot, loaded_nvidia.as_deref(), &installed)
}

/// Package that provides the running kernel; its modules directory carries a `pkgbase` file.
fn running_kernel_package(modules: &Path, release: &str) -> Option<String> {
    let base = fs::read_to_string(modules.join(release).join("pkgbase")).ok()?;
    Some(base.trim().to_string()).filter(|b| !b.is_empty())
}

/// Packages whose new version only takes effect after a reboot, whatever kernel is running.
/// Union of the lists in the EndeavourOS, CachyOS and Manjaro reboot hooks and alpm-reboot-required.
fn boot_bound(name: &str) -> bool {
    const EXACT: &[&str] = &[
        "systemd", "glibc", "dbus", "dbus-broker", "mesa", "wayland", "egl-wayland",
        "cryptsetup", "btrfs-progs", "e2fsprogs", "xfsprogs", "grub", "mkinitcpio", "booster", "dracut",
        "virtualbox-guest-utils", "ca-certificates", "gcc-libs", "glib2", "libcap", "openssl", "zlib",
    ];
    const PREFIXES: &[&str] = &["systemd-", "linux-firmware", "mkinitcpio-", "dracut-", "booster-", "xf86-video-", "xorg-server", "xorg-fonts"];
    const NOT_DRIVERS: &[&str] = &["nvidia-settings", "nvidia-prime", "nvidia-container-toolkit", "nvidia-docker"];
    EXACT.contains(&name)
        || PREFIXES.iter().any(|p| name.starts_with(p))
        || name.ends_with("-ucode")
        || (name.ends_with("-firmware") && !name.starts_with("qemu-"))
        || name.ends_with("-dkms")
        || name.contains("-host-modules")
        || (name.contains("nvidia") && !NOT_DRIVERS.contains(&name))
}

#[derive(Debug, PartialEq, Eq)]
struct Installed {
    name: String,
    version: String,
    date: i64,
}

fn pending(release: &str, modules: &Path, boot: Option<i64>, loaded_nvidia: Option<&str>, installed: &[Installed]) -> bool {
    if !release.is_empty() && !modules.join(release).exists() {
        return true;
    }
    if let Some(boot) = boot
        && installed.iter().any(|p| p.date > boot && !p.name.contains("nvidia"))
    {
        return true;
    }
    match (loaded_nvidia.map(str::trim), installed.iter().find(|p| p.name == "nvidia-utils")) {
        (Some(loaded), Some(utils)) => driver_version(&utils.version) != loaded,
        _ => false,
    }
}

/// `1:570.133.07-2` -> `570.133.07`
fn driver_version(pkg_version: &str) -> &str {
    let no_epoch = pkg_version.split_once(':').map_or(pkg_version, |(_, v)| v);
    no_epoch.rsplit_once('-').map_or(no_epoch, |(v, _)| v)
}

fn boot_time(proc_stat: &str) -> Option<i64> {
    proc_stat.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())
}

/// Boot-bound entries of pacman's local database, with their install dates.
fn installed_packages(local: &Path) -> Vec<Installed> {
    let Ok(entries) = fs::read_dir(local) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let dir = e.file_name().into_string().ok()?;
            let mut parts = dir.rsplitn(3, '-');
            let (rel, ver, name) = (parts.next()?, parts.next()?, parts.next()?);
            if !boot_bound(name) {
                return None;
            }
            let desc = fs::read_to_string(e.path().join("desc")).ok()?;
            Some(Installed { name: name.to_string(), version: format!("{ver}-{rel}"), date: install_date(&desc)? })
        })
        .collect()
}

fn install_date(desc: &str) -> Option<i64> {
    let mut lines = desc.lines();
    lines.find(|l| *l == "%INSTALLDATE%")?;
    lines.next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(std::path::PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("xpm-reboot-{}-{tag}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Dir(path)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pkg(name: &str, version: &str, date: i64) -> Installed {
        Installed { name: name.into(), version: version.into(), date }
    }

    #[test]
    fn running_kernel_comes_from_its_modules_dir() {
        let dir = Dir::new("pkgbase");
        fs::create_dir_all(dir.0.join("7.3.1-1-cachyos")).unwrap();
        fs::write(dir.0.join("7.3.1-1-cachyos/pkgbase"), "linux-cachyos\n").unwrap();
        assert_eq!(running_kernel_package(&dir.0, "7.3.1-1-cachyos").as_deref(), Some("linux-cachyos"));
        assert_eq!(running_kernel_package(&dir.0, "7.3.4-1-cachyos"), None);
    }

    #[test]
    fn boot_bound_packages() {
        for n in ["amd-ucode", "intel-ucode", "linux-firmware", "linux-firmware-amdgpu", "sof-firmware", "alsa-firmware",
                  "nvidia-utils", "nvidia-open-dkms", "lib32-nvidia-utils", "nvidia-580xx-utils", "r8168-dkms",
                  "virtualbox-host-modules-arch", "systemd", "systemd-libs", "glibc", "dbus-broker", "mesa", "wayland",
                  "xf86-video-amdgpu", "xf86-video-nouveau", "xorg-server", "mkinitcpio", "mkinitcpio-busybox", "dracut",
                  "cryptsetup", "grub", "gcc-libs", "libcap", "ca-certificates", "openssl", "zlib", "glib2"] {
            assert!(boot_bound(n), "{n}");
        }
        for n in ["nvidia-settings", "nvidia-prime", "vulkan-radeon", "lib32-mesa", "linux-headers", "linux-docs", "bash", "fwupd",
                  "extra-cmake-modules", "qemu-system-x86-firmware", "xorg-xwayland", "lib32-openssl"] {
            assert!(!boot_bound(n), "{n}");
        }
    }

    #[test]
    fn replaced_kernel_is_pending() {
        let dir = Dir::new("kernel");
        fs::create_dir_all(dir.0.join("7.3.4-1-cachyos")).unwrap();
        assert!(pending("7.3.1-1-cachyos", &dir.0, None, None, &[]));
        assert!(!pending("7.3.4-1-cachyos", &dir.0, None, None, &[]));
    }

    #[test]
    fn boot_bound_install_after_boot_is_pending() {
        let dir = Dir::new("since-boot");
        fs::create_dir_all(dir.0.join("k")).unwrap();
        assert!(pending("k", &dir.0, Some(100), None, &[pkg("amd-ucode", "1-1", 200)]));
        assert!(!pending("k", &dir.0, Some(100), None, &[pkg("nvidia-utils", "570.144-1", 200)]));
        assert!(!pending("k", &dir.0, Some(100), None, &[pkg("amd-ucode", "1-1", 50)]));
    }

    #[test]
    fn nvidia_userspace_must_match_loaded_module() {
        let dir = Dir::new("nvidia");
        fs::create_dir_all(dir.0.join("k")).unwrap();
        let pkgs = [pkg("nvidia-utils", "1:570.144-2", 10)];
        assert!(pending("k", &dir.0, Some(100), Some("570.133.07\n"), &pkgs));
        assert!(!pending("k", &dir.0, Some(100), Some("570.144\n"), &pkgs));
        assert!(!pending("k", &dir.0, Some(100), None, &pkgs));
    }

    #[test]
    fn parses_boot_time_and_install_date() {
        assert_eq!(boot_time("cpu 1 2\nbtime 1794309689\nprocesses 5\n"), Some(1794309689));
        assert_eq!(install_date("%NAME%\namd-ucode\n\n%INSTALLDATE%\n1793595055\n\n%SIZE%\n9\n"), Some(1793595055));
        assert_eq!(install_date("%NAME%\nx\n"), None);
    }

    #[test]
    fn reads_the_local_database() {
        let dir = Dir::new("db");
        for (entry, date) in [("amd-ucode-20261019-1", "300"), ("bash-5.3-1", "300"), ("linux-firmware-amdgpu-20261019-1", "bad")] {
            fs::create_dir_all(dir.0.join(entry)).unwrap();
            fs::write(dir.0.join(entry).join("desc"), format!("%INSTALLDATE%\n{date}\n")).unwrap();
        }
        assert_eq!(installed_packages(&dir.0), vec![pkg("amd-ucode", "20261019-1", 300)]);
    }
}
