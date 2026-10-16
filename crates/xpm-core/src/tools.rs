//! Locating external helper tools, preferring a system copy on `PATH` and
//! falling back to the bundled copies xpm ships in `/usr/lib/xpm` (built from
//! source by the package when the host repos don't provide them).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Directory holding xpm's bundled fallback tools (e.g. `downgrade`,
/// `appimageupdatetool`) installed by the package on systems whose repos do not
/// provide them. A system copy on `PATH` always takes precedence.
pub const BUNDLED_TOOL_DIR: &str = "/usr/lib/xpm";

/// Resolve `name` to an executable path: a copy on `PATH` wins, otherwise the
/// bundled copy in [`BUNDLED_TOOL_DIR`]. Returns `None` if neither exists.
pub fn resolve_tool(name: &str) -> Option<PathBuf> {
    resolve_tool_in(name, std::env::var_os("PATH"), Path::new(BUNDLED_TOOL_DIR))
}

fn resolve_tool_in(name: &str, path_var: Option<OsString>, bundled_dir: &Path) -> Option<PathBuf> {
    if let Some(paths) = path_var
        && let Some(hit) = std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| p.is_file())
        {
            return Some(hit);
        }
    let bundled = bundled_dir.join(name);
    bundled.is_file().then_some(bundled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(sub: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xpm-tools-{}-{}", std::process::id(), sub));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch_exec(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn prefers_path_over_bundled() {
        let pathdir = tmp("path");
        let bundled = tmp("bundled");
        let on_path = touch_exec(&pathdir, "footool");
        touch_exec(&bundled, "footool");
        let got = resolve_tool_in("footool", Some(pathdir.into_os_string()), &bundled);
        assert_eq!(got, Some(on_path));
    }

    #[test]
    fn falls_back_to_bundled() {
        let bundled = tmp("only-bundled");
        let b = touch_exec(&bundled, "bartool");
        let got = resolve_tool_in("bartool", Some(tmp("empty").into_os_string()), &bundled);
        assert_eq!(got, Some(b));
    }

    #[test]
    fn none_when_missing() {
        let bundled = tmp("nope");
        let got = resolve_tool_in("ghosttool", Some(tmp("empty2").into_os_string()), &bundled);
        assert_eq!(got, None);
    }
}
