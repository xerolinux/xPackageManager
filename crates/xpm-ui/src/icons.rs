use crate::appstream::CachedRemoteApp;
use crate::xml::{attr_value, XmlEvents};
use quick_xml::events::Event;
use std::collections::HashMap;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const SYSTEM_CATALOGS: &[&str] = &["usr/share/swcatalog", "usr/share/app-info"];
const CATALOG_PACKAGE: &str = "archlinux-appstream-data";
const CATALOG_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 3600);
const ARCH_KEYRING: &str = "/usr/share/pacman/keyrings/archlinux.gpg";
const THEMES: &[&str] = &["hicolor", "breeze", "Adwaita", "AdwaitaLegacy"];

type Candidates = HashMap<String, Vec<(u32, PathBuf)>>;

/// Package name -> icon file for every package that ships a launcher, plus catalog
/// icons for repo packages (system AppStream catalog and `extra_catalogs`).
pub(crate) fn build_index(root: &Path, extra_catalogs: &[PathBuf]) -> HashMap<String, PathBuf> {
    let themes = scan_themes(root);
    let bases: Vec<PathBuf> = SYSTEM_CATALOGS
        .iter()
        .map(|dir| root.join(dir))
        .chain(extra_catalogs.iter().cloned())
        .collect();
    let mut index = catalog_icons(&bases, &themes);
    index.extend(installed_icons(root, &themes));
    index
}

pub(crate) fn catalog_cache_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/xpm/appstream-data")
}

/// Catalog base directories inside the user cache populated by `ensure_catalog_cache`.
pub(crate) fn cached_catalogs(cache: &Path) -> Vec<PathBuf> {
    vec![cache.join("usr/share/swcatalog")]
}

fn system_catalog_present(root: &Path) -> bool {
    SYSTEM_CATALOGS.iter().any(|dir| {
        ["xml", "xmls"].iter().any(|sub| {
            fs::read_dir(root.join(dir).join(sub)).is_ok_and(|mut entries| entries.next().is_some())
        })
    })
}

fn cache_is_fresh(cache: &Path) -> bool {
    fs::metadata(cache.join(".stamp"))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < CATALOG_MAX_AGE)
        && cache.join("usr/share/swcatalog").is_dir()
}

fn run(cmd: &mut Command) -> bool {
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Download and unpack `archlinux-appstream-data` into `cache` when the system does not
/// already have an AppStream catalog. The package signature is verified against the Arch
/// keyring before anything is unpacked. Returns true when new catalog data was installed.
pub(crate) fn ensure_catalog_cache(root: &Path, cache: &Path) -> bool {
    if system_catalog_present(root) || cache_is_fresh(cache) {
        return false;
    }
    let Ok(out) = Command::new("pacman").args(["-Sp", CATALOG_PACKAGE]).output() else { return false };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let Some(url) = stdout.lines().map(str::trim).find(|l| l.starts_with("http")) else { return false };

    let work = cache.with_extension("part");
    let _ = fs::remove_dir_all(&work);
    if fs::create_dir_all(&work).is_err() {
        return false;
    }
    let package = work.join("catalog.pkg.tar.zst");
    let signature = work.join("catalog.pkg.tar.zst.sig");
    let fetched = run(Command::new("curl").args(["-sfL", "--max-time", "180", "-o"]).arg(&package).arg(url))
        && run(Command::new("curl").args(["-sfL", "--max-time", "30", "-o"]).arg(&signature).arg(format!("{url}.sig")));
    // gpgv only reads binary keyrings; the Arch keyring ships ASCII-armored.
    let ring = work.join("keyring.gpg");
    let verified = fetched
        && run(Command::new("gpg")
            .arg("--homedir")
            .arg(&work)
            .args(["--batch", "--dearmor", "--output"])
            .arg(&ring)
            .arg(ARCH_KEYRING))
        && run(Command::new("gpgv")
            .arg("--homedir")
            .arg(&work)
            .arg("--keyring")
            .arg(&ring)
            .arg(&signature)
            .arg(&package));
    let staging = work.join("unpacked");
    let unpacked = verified
        && fs::create_dir_all(&staging).is_ok()
        && run(Command::new("bsdtar")
            .arg("-xf")
            .arg(&package)
            .arg("-C")
            .arg(&staging)
            .arg("usr/share/swcatalog"));
    let installed = unpacked && {
        let _ = fs::remove_dir_all(cache);
        fs::create_dir_all(cache).is_ok()
            && fs::rename(staging.join("usr"), cache.join("usr")).is_ok()
            && fs::write(cache.join(".stamp"), b"").is_ok()
    };
    let _ = fs::remove_dir_all(&work);
    installed
}

fn size_rank(dir_name: &str) -> u32 {
    let n: u32 = dir_name
        .split(['x', '@'])
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    match n {
        128 => 0,
        96 => 1,
        64 => 2,
        256 => 3,
        48 => 4,
        512 => 5,
        32 => 6,
        24 => 7,
        16 => 8,
        _ => 9,
    }
}

fn is_image(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some("png" | "svg"))
}

fn walk_theme(dir: &Path, rank: u32, found: &mut Candidates) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            let child_rank = if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                size_rank(&name)
            } else if name == "scalable" {
                10
            } else {
                rank
            };
            walk_theme(&path, child_rank, found);
        } else if is_image(&path)
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            let in_apps = dir.components().any(|c| c.as_os_str() == "apps");
            let symbolic = if stem.ends_with("-symbolic") { 40 } else { 0 };
            let rank = rank + symbolic + if in_apps { 0 } else { 20 };
            found.entry(stem.to_string()).or_default().push((rank, path));
        }
    }
}

fn scan_themes(root: &Path) -> Candidates {
    let mut found = Candidates::new();
    for theme in THEMES {
        walk_theme(&root.join("usr/share/icons").join(theme), 9, &mut found);
    }
    if let Ok(entries) = fs::read_dir(root.join("usr/share/pixmaps")) {
        for entry in entries.flatten() {
            let path = entry.path();
            if is_image(&path)
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                found.entry(stem.to_string()).or_default().push((11, path));
            }
        }
    }
    for list in found.values_mut() {
        list.sort_by_key(|(rank, _)| *rank);
    }
    found
}

fn resolve(root: &Path, icon: &str, themes: &Candidates) -> Option<PathBuf> {
    if icon.starts_with('/') {
        let path = root.join(icon.trim_start_matches('/'));
        return (is_image(&path) && path.is_file()).then_some(path);
    }
    let stem = [".png", ".svg", ".xpm"]
        .iter()
        .find_map(|ext| icon.strip_suffix(ext))
        .unwrap_or(icon);
    themes.get(stem).and_then(|list| list.first()).map(|(_, p)| p.clone())
}

fn desktop_icon(text: &str) -> Option<String> {
    let mut in_entry = false;
    let mut icon = None;
    let mut is_app = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some((key, value)) = line.split_once('=') {
            match key {
                "Type" => is_app = value == "Application",
                "Icon" => icon = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    icon.filter(|i| is_app && !i.is_empty())
}

fn package_name(dir_name: &str) -> Option<&str> {
    let without_release = dir_name.rsplit_once('-')?.0;
    Some(without_release.rsplit_once('-')?.0)
}

fn installed_icons(root: &Path, themes: &Candidates) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    let Ok(entries) = fs::read_dir(root.join("var/lib/pacman/local")) else { return index };
    for entry in entries.flatten() {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let Some(name) = package_name(&dir_name) else { continue };
        let Ok(files) = fs::read_to_string(entry.path().join("files")) else { continue };
        let icon = files
            .lines()
            .filter(|l| {
                l.strip_prefix("usr/share/applications/")
                    .is_some_and(|rest| rest.ends_with(".desktop") && !rest.contains('/'))
            })
            .filter_map(|l| fs::read_to_string(root.join(l)).ok())
            .filter_map(|text| desktop_icon(&text))
            .find_map(|icon| resolve(root, &icon, themes));
        if let Some(path) = icon {
            index.insert(name.to_string(), path);
        }
    }
    index
}

fn catalog_icons(bases: &[PathBuf], themes: &Candidates) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    for base in bases {
        for sub in ["xml", "xmls"] {
            let Ok(entries) = fs::read_dir(base.join(sub)) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file) = fs::File::open(&path) else { continue };
                if path.extension().and_then(|e| e.to_str()) == Some("gz") {
                    parse_catalog(base, themes, BufReader::new(flate2::read::GzDecoder::new(file)), &mut index);
                } else {
                    parse_catalog(base, themes, BufReader::new(file), &mut index);
                }
            }
        }
    }
    index
}

#[derive(Default)]
struct Component {
    pkgname: String,
    cached: Option<(u32, String)>,
    stock: String,
}

fn parse_catalog<R: std::io::BufRead>(
    base: &Path,
    themes: &Candidates,
    source: R,
    index: &mut HashMap<String, PathBuf>,
) {
    let mut events = XmlEvents::new(source);
    let mut origin = String::new();
    let mut current: Option<Component> = None;
    let mut tag = String::new();
    let mut icon_kind = String::new();
    let mut icon_width = 0u32;
    loop {
        match events.next() {
            Event::Start(e) => {
                tag = e.name().as_ref().to_string();
                match tag.as_str() {
                    "components" => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == "origin" {
                                origin = attr_value(&attr);
                            }
                        }
                    }
                    "component" => current = Some(Component::default()),
                    "icon" => {
                        icon_kind.clear();
                        icon_width = 0;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                "type" => icon_kind = attr_value(&attr),
                                "width" => icon_width = attr_value(&attr).parse().unwrap_or(0),
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) => {
                let text = t.xml_content(quick_xml::XmlVersion::Implicit1_0).trim().to_string();
                if let Some(comp) = current.as_mut() {
                    match tag.as_str() {
                        "pkgname" if comp.pkgname.is_empty() => comp.pkgname = text,
                        "icon" if icon_kind == "cached" => {
                            let better = comp
                                .cached
                                .as_ref()
                                .is_none_or(|(w, _)| size_rank(&icon_width.to_string()) < size_rank(&w.to_string()));
                            if better {
                                comp.cached = Some((icon_width, text));
                            }
                        }
                        "icon" if icon_kind == "stock" => comp.stock = text,
                        _ => {}
                    }
                }
            }
            Event::End(e) => {
                tag.clear();
                if e.name().as_ref() == "component"
                    && let Some(comp) = current.take()
                    && !comp.pkgname.is_empty()
                {
                    let from_cache = comp.cached.and_then(|(w, file)| {
                        let path = base.join("icons").join(&origin).join(format!("{w}x{w}")).join(file);
                        path.is_file().then_some(path)
                    });
                    let icon = from_cache.or_else(|| {
                        (!comp.stock.is_empty()).then(|| resolve(base, &comp.stock, themes)).flatten()
                    });
                    if let Some(path) = icon {
                        index.entry(comp.pkgname).or_insert(path);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
}


fn to_rgba8(channels: usize, buf: &[f32]) -> Vec<u8> {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut out = Vec::with_capacity(buf.len() / channels * 4);
    for px in buf.chunks_exact(channels) {
        out.extend_from_slice(&match channels {
            1 => [byte(px[0]), byte(px[0]), byte(px[0]), 255],
            2 => [byte(px[0]), byte(px[0]), byte(px[0]), byte(px[1])],
            3 => [byte(px[0]), byte(px[1]), byte(px[2]), 255],
            _ => [byte(px[0]), byte(px[1]), byte(px[2]), byte(px[3])],
        });
    }
    out
}

fn decode_jxl(path: &Path) -> Option<Pixels> {
    let image = jxl_oxide::JxlImage::builder().open(path).ok()?;
    let frame = image.render_frame(0).ok()?.image_all_channels();
    let (width, height) = (frame.width() as u32, frame.height() as u32);
    if width == 0 || height == 0 || frame.channels() == 0 {
        return None;
    }
    Some(Pixels { width, height, rgba: to_rgba8(frame.channels(), frame.buf()) })
}

/// Decode an icon file to pixels off the UI thread. Handles JPEG XL (Arch's AppStream
/// catalog) and the raster formats of the `image` crate; SVG returns `None`.
pub(crate) fn decode_file(path: &Path) -> Option<Pixels> {
    if path.extension().and_then(|e| e.to_str()) == Some("jxl") {
        return decode_jxl(path);
    }
    image::open(path).ok().map(|img| pixels_of(&img.thumbnail(256, 256)))
}

pub(crate) fn is_svg(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("svg")
}

const THUMB_SIZE: u32 = 96;

/// Decoded icon pixels, safe to send between threads.
pub(crate) struct Pixels {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Pixels {
    pub(crate) fn into_image(self) -> slint::Image {
        slint::Image::from_rgba8(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &self.rgba, self.width, self.height,
        ))
    }
}

fn pixels_of(img: &image::DynamicImage) -> Pixels {
    let rgba = img.to_rgba8();
    Pixels { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() }
}

/// Where the downscaled copy of a downloaded icon lives.
pub(crate) fn thumb_path(original: &Path) -> PathBuf {
    let stem = original.file_stem().and_then(|s| s.to_str()).unwrap_or("icon");
    original.with_file_name(format!("{stem}.t{THUMB_SIZE}.png"))
}

/// Decode an existing thumbnail (cheap; meant for worker threads).
pub(crate) fn read_thumb(thumb: &Path) -> Option<Pixels> {
    image::open(thumb).ok().map(|img| pixels_of(&img))
}

/// Decode a full-size downloaded icon off the UI thread, keep a small PNG copy and drop
/// the original. Formats the `image` crate cannot read (SVG) return `None` and are left alone.
pub(crate) fn make_thumb(original: &Path, thumb: &Path) -> Option<Pixels> {
    let img = image::open(original).ok()?;
    let small = img.thumbnail(THUMB_SIZE, THUMB_SIZE);
    if small.save(thumb).is_ok() {
        let _ = fs::remove_file(original);
    }
    Some(pixels_of(&small))
}

fn appstream_dir(remote: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let user = PathBuf::from(&home).join(format!(".local/share/flatpak/appstream/{remote}/x86_64/active"));
    if user.join("appstream.xml").exists() || user.join("appstream.xml.gz").exists() {
        return user;
    }
    PathBuf::from(format!("/var/lib/flatpak/appstream/{remote}/x86_64/active"))
}

const EXPORT_SIZES: &[&str] = &["128x128", "256x256", "64x64", "512x512", "48x48", "scalable"];

/// Exported launcher icon of an installed Flatpak under the given `exports/share/icons/hicolor` roots.
fn flatpak_export_icon(roots: &[PathBuf], app_id: &str) -> Option<PathBuf> {
    roots.iter().find_map(|root| {
        EXPORT_SIZES.iter().find_map(|size| {
            ["png", "svg"]
                .iter()
                .map(|ext| root.join(size).join("apps").join(format!("{app_id}.{ext}")))
                .find(|path| path.is_file())
        })
    })
}

/// Icon file for a Flatpak app: AppStream 128px icon of the loaded remote (or flathub),
/// then the icon the installed app exports.
pub(crate) fn flatpak_icon(store: &[CachedRemoteApp], remote: &str, app_id: &str) -> Option<PathBuf> {
    if let Some(app) = store.iter().find(|a| a.app_id == app_id)
        && !app.icon_name.is_empty()
    {
        for name in [remote, "flathub"].into_iter().filter(|r| !r.is_empty()) {
            let path = appstream_dir(name).join("icons/128x128").join(&app.icon_name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let roots = [
        PathBuf::from(home).join(".local/share/flatpak/exports/share/icons/hicolor"),
        PathBuf::from("/var/lib/flatpak/exports/share/icons/hicolor"),
    ];
    flatpak_export_icon(&roots, app_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("xpm-icons-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn parses_launcher_icon_only_for_applications() {
        let text = "[Desktop Entry]\nType=Application\nName=X\nIcon=foo\n[Desktop Action a]\nIcon=other\n";
        assert_eq!(desktop_icon(text).as_deref(), Some("foo"));
        assert_eq!(desktop_icon("[Desktop Entry]\nType=Link\nIcon=foo\n"), None);
        assert_eq!(desktop_icon("[Desktop Entry]\nType=Application\n"), None);
    }

    #[test]
    fn strips_version_and_release_from_db_dir() {
        assert_eq!(package_name("firefox-150.0.1-1"), Some("firefox"));
        assert_eq!(package_name("lib32-mesa-26.0.3-2"), Some("lib32-mesa"));
        assert_eq!(package_name("broken"), None);
    }

    #[test]
    fn prefers_128_png_over_small_and_scalable() {
        let root = tree("rank");
        write(&root, "usr/share/icons/hicolor/16x16/apps/app.png", "");
        write(&root, "usr/share/icons/hicolor/128x128/apps/app.png", "");
        write(&root, "usr/share/icons/hicolor/scalable/apps/app.svg", "");
        let themes = scan_themes(&root);
        let picked = resolve(&root, "app", &themes).unwrap();
        assert!(picked.ends_with("128x128/apps/app.png"), "{picked:?}");
    }

    #[test]
    fn maps_installed_package_through_its_desktop_file() {
        let root = tree("installed");
        write(&root, "var/lib/pacman/local/demo-1.0-1/files", "%FILES%\nusr/\nusr/share/applications/demo.desktop\n");
        write(&root, "usr/share/applications/demo.desktop", "[Desktop Entry]\nType=Application\nIcon=demo-icon\n");
        write(&root, "usr/share/icons/hicolor/64x64/apps/demo-icon.png", "");
        write(&root, "var/lib/pacman/local/lib-2.0-1/files", "%FILES%\nusr/lib/libx.so\n");
        let index = build_index(&root, &[]);
        assert!(index["demo"].ends_with("64x64/apps/demo-icon.png"));
        assert!(!index.contains_key("lib"));
    }

    #[test]
    fn dotted_icon_names_are_not_treated_as_extensions() {
        let root = tree("dotted");
        write(&root, "usr/share/icons/hicolor/scalable/apps/org.kde.dolphin.svg", "");
        let themes = scan_themes(&root);
        assert!(resolve(&root, "org.kde.dolphin", &themes).is_some());
        assert!(resolve(&root, "org.kde.dolphin.svg", &themes).is_some());
    }

    #[test]
    fn flatpak_export_icons_prefer_png_sizes_in_order() {
        let root = tree("flatpak");
        let hicolor = root.join("hicolor");
        write(&root, "hicolor/64x64/apps/org.x.App.png", "");
        write(&root, "hicolor/scalable/apps/org.x.App.svg", "");
        let found = flatpak_export_icon(std::slice::from_ref(&hicolor), "org.x.App").unwrap();
        assert!(found.ends_with("64x64/apps/org.x.App.png"));
        write(&root, "hicolor/128x128/apps/org.x.App.png", "");
        let found = flatpak_export_icon(std::slice::from_ref(&hicolor), "org.x.App").unwrap();
        assert!(found.ends_with("128x128/apps/org.x.App.png"));
        assert!(flatpak_export_icon(&[hicolor], "org.missing.App").is_none());
    }

    #[test]
    fn converts_float_channels_to_rgba8() {
        assert_eq!(to_rgba8(4, &[1.0, 0.0, 0.5, 0.25]), [255, 0, 128, 64]);
        assert_eq!(to_rgba8(1, &[0.5]), [128, 128, 128, 255]);
        assert_eq!(to_rgba8(3, &[0.0, 1.0, 0.0]), [0, 255, 0, 255]);
        assert_eq!(to_rgba8(2, &[1.0, 0.0]), [255, 255, 255, 0]);
        assert_eq!(to_rgba8(4, &[]), Vec::<u8>::new());
    }

    #[test]
    fn thumbnails_are_downscaled_and_replace_the_original() {
        let root = tree("thumb");
        let original = root.join("big.png");
        image::RgbaImage::from_pixel(400, 200, image::Rgba([10, 20, 30, 255])).save(&original).unwrap();
        let thumb = thumb_path(&original);
        assert!(thumb.ends_with("big.t96.png"));
        let px = make_thumb(&original, &thumb).unwrap();
        assert_eq!((px.width, px.height), (96, 48));
        assert!(!original.exists() && thumb.exists());
        let again = read_thumb(&thumb).unwrap();
        assert_eq!((again.width, again.height), (96, 48));
        assert_eq!(&again.rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn undecodable_icons_keep_their_original() {
        let root = tree("svg");
        let original = root.join("logo.svg");
        fs::write(&original, "<svg/>").unwrap();
        assert!(make_thumb(&original, &thumb_path(&original)).is_none());
        assert!(original.exists());
    }

    #[test]
    fn absolute_icon_paths_resolve_under_root() {
        let root = tree("abs");
        write(&root, "opt/app/logo.png", "");
        assert!(resolve(&root, "/opt/app/logo.png", &Candidates::new()).is_some());
        assert!(resolve(&root, "/opt/app/missing.png", &Candidates::new()).is_none());
    }

    #[test]
    fn catalog_supplies_icons_for_uninstalled_packages() {
        let root = tree("catalog");
        write(
            &root,
            "usr/share/swcatalog/xml/extra.xml",
            r#"<components version="0.16" origin="archlinux-arch-extra">
<component type="desktop-application"><id>org.x.Y</id><pkgname>ypkg</pkgname>
<icon type="cached" width="64" height="64">ypkg_y.png</icon></component>
<component type="addon"><pkgname>noicon</pkgname></component>
</components>"#,
        );
        write(&root, "usr/share/swcatalog/icons/archlinux-arch-extra/64x64/ypkg_y.png", "");
        let index = build_index(&root, &[]);
        assert!(index["ypkg"].ends_with("archlinux-arch-extra/64x64/ypkg_y.png"));
        assert!(!index.contains_key("noicon"));
    }

    #[test]
    fn installed_icon_wins_over_catalog() {
        let root = tree("precedence");
        write(&root, "var/lib/pacman/local/dual-1-1/files", "%FILES%\nusr/share/applications/dual.desktop\n");
        write(&root, "usr/share/applications/dual.desktop", "[Desktop Entry]\nType=Application\nIcon=dual\n");
        write(&root, "usr/share/icons/hicolor/64x64/apps/dual.png", "");
        write(
            &root,
            "usr/share/swcatalog/xml/extra.xml",
            r#"<components origin="o"><component type="desktop-application"><pkgname>dual</pkgname><icon type="cached" width="64" height="64">d.png</icon></component></components>"#,
        );
        write(&root, "usr/share/swcatalog/icons/o/64x64/d.png", "");
        assert!(build_index(&root, &[])["dual"].ends_with("hicolor/64x64/apps/dual.png"));
    }
}


