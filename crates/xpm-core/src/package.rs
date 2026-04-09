use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Version {
    pub full: String,
    pub epoch: Option<u32>,
    pub pkgver: String,
    pub pkgrel: String,
}

impl Version {
    pub fn new(version_str: &str) -> Self {
        let (epoch, rest) = if let Some(idx) = version_str.find(':') {
            let epoch = version_str[..idx].parse().ok();
            (epoch, &version_str[idx + 1..])
        } else {
            (None, version_str)
        };

        let (pkgver, pkgrel) = if let Some(idx) = rest.rfind('-') {
            (rest[..idx].to_string(), rest[idx + 1..].to_string())
        } else {
            (rest.to_string(), String::new())
        };

        Self {
            full: version_str.to_string(),
            epoch,
            pkgver,
            pkgrel,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.full)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // epoch always wins
        match (self.epoch, other.epoch) {
            (Some(a), Some(b)) => match a.cmp(&b) {
                Ordering::Equal => {}
                ord => return ord,
            },
            (Some(_), None) => return Ordering::Greater,
            (None, Some(_)) => return Ordering::Less,
            (None, None) => {}
        }

        match vercmp(&self.pkgver, &other.pkgver) {
            Ordering::Equal => {}
            ord => return ord,
        }

        vercmp(&self.pkgrel, &other.pkgrel)
    }
}

// pacman-style version comparision logic
fn vercmp(a: &str, b: &str) -> Ordering {
    let mut a_chars = a.chars().peekable();
    let mut b_chars = b.chars().peekable();

    loop {
        while a_chars.peek().is_some_and(|c| !c.is_alphanumeric()) {
            a_chars.next();
        }
        while b_chars.peek().is_some_and(|c| !c.is_alphanumeric()) {
            b_chars.next();
        }

        match (a_chars.peek().copied(), b_chars.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ac), Some(bc)) => {
                let a_is_digit = ac.is_ascii_digit();
                let b_is_digit = bc.is_ascii_digit();

                match (a_is_digit, b_is_digit) {
                    (true, true) => {
                        let mut a_num = String::new();
                        while let Some(&c) = a_chars.peek() {
                            if c.is_ascii_digit() {
                                a_num.push(c);
                                a_chars.next();
                            } else {
                                break;
                            }
                        }
                        let mut b_num = String::new();
                        while let Some(&c) = b_chars.peek() {
                            if c.is_ascii_digit() {
                                b_num.push(c);
                                b_chars.next();
                            } else {
                                break;
                            }
                        }

                        match a_num.len().cmp(&b_num.len()) {
                            Ordering::Equal => match a_num.cmp(&b_num) {
                                Ordering::Equal => continue,
                                ord => return ord,
                            },
                            ord => return ord,
                        }
                    }
                    (false, false) => {
                        let mut a_alpha = String::new();
                        while let Some(&c) = a_chars.peek() {
                            if c.is_alphabetic() {
                                a_alpha.push(c);
                                a_chars.next();
                            } else {
                                break;
                            }
                        }
                        let mut b_alpha = String::new();
                        while let Some(&c) = b_chars.peek() {
                            if c.is_alphabetic() {
                                b_alpha.push(c);
                                b_chars.next();
                            } else {
                                break;
                            }
                        }

                        match a_alpha.cmp(&b_alpha) {
                            Ordering::Equal => continue,
                            ord => return ord,
                        }
                    }
                    (true, false) => return Ordering::Greater,
                    (false, true) => return Ordering::Less,
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PackageBackend {
    Pacman,
    Flatpak,
}

impl fmt::Display for PackageBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageBackend::Pacman => write!(f, "pacman"),
            PackageBackend::Flatpak => write!(f, "flatpak"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PackageStatus {
    Installed,
    Available,
    Upgradable,
    Orphan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: Version,
    pub description: String,
    pub backend: PackageBackend,
    pub status: PackageStatus,
    pub repository: String,
}

impl Package {
    pub fn new(
        name: impl Into<String>,
        version: Version,
        description: impl Into<String>,
        backend: PackageBackend,
        status: PackageStatus,
        repository: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            version,
            description: description.into(),
            backend,
            status,
            repository: repository.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInfo {
    pub package: Package,
    pub url: Option<String>,
    pub licenses: Vec<String>,
    pub groups: Vec<String>,
    pub depends: Vec<String>,
    pub optdepends: Vec<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
    pub replaces: Vec<String>,
    pub installed_size: u64,
    pub download_size: u64,
    pub build_date: Option<DateTime<Utc>>,
    pub install_date: Option<DateTime<Utc>>,
    pub packager: Option<String>,
    pub arch: String,
    pub reason: Option<InstallReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum InstallReason {
    Explicit,
    Dependency,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub name: String,
    pub version: Version,
    pub description: String,
    pub backend: PackageBackend,
    pub repository: String,
    pub installed: bool,
    pub installed_version: Option<Version>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub name: String,
    pub current_version: Version,
    pub new_version: Version,
    pub backend: PackageBackend,
    pub repository: String,
    pub download_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version_comparison() {
        let v1 = Version::new("1.0.0-1");
        let v2 = Version::new("1.0.1-1");
        let v3 = Version::new("1:0.5.0-1");
        let v4 = Version::new("2.0.0-1");

        assert!(v1 < v2);
        assert!(v2 < v4);
        assert!(v3 > v4);
        assert!(v1 == Version::new("1.0.0-1"));
    }

    #[test]
    fn test_version_parsing() {
        let v = Version::new("1:2.3.4-5");
        assert_eq!(v.epoch, Some(1));
        assert_eq!(v.pkgver, "2.3.4");
        assert_eq!(v.pkgrel, "5");
    }
}
