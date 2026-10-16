# Native Package Icons + Flathub-Style Flatpak Detail Page - Design

Date: 2026-10-16
Status: Approved by user in chat (design choices below)

## Goals

1. Native (pacman) packages show proper application icons wherever they are listed, when an icon exists.
2. The Flatpak detail page looks like the flathub.org app page: screenshot carousel, information in rounded cards, release notes, links. The Add-Ons button and its modal are untouched.

## Decisions (locked)

- Icons for packages that are not installed come from `archlinux-appstream-data` when it is present (optional; added as `optdepends`). No new hard dependency. Installed packages never need it.
- The Flatpak page is enriched from the Flathub web API (download size, installed size, verified badge, install count) for the `flathub` remote only. Failure or no network leaves those fields hidden; the page never waits on the network.
- Verification is by screenshots of static states. No synthetic input against the running app.

## A. Native icons

- New module `crates/xpm-ui/src/icons.rs`.
- `IconIndex` is built once on a background thread at startup and published through an `Arc<RwLock<HashMap<String, PathBuf>>>` (package name to icon file):
  - Installed packages: read `/var/lib/pacman/local/<pkg>-<ver>/files`, collect `usr/share/applications/*.desktop`, parse `Type`, `NoDisplay`, `Icon`, and resolve `Icon` to a file.
  - Resolution order: absolute path; the package's own hicolor entries (prefer 128, 96, 64, 256, 48, then scalable svg); `/usr/share/pixmaps`; the system icon themes (hicolor, then breeze, Adwaita).
  - Catalog (optional): if `/usr/share/swcatalog/xml/*.xml.gz` exists, map `<pkgname>` to its cached icon in `/usr/share/swcatalog/icons/<origin>/<size>/`; installed-package icons take precedence.
- UI integration without changing `PackageData` (which must stay `Send`):
  - A Slint `pure callback icon-for(string) -> image` implemented on the UI thread with a `HashMap<String, slint::Image>` cache. Images decode lazily, only for rows that are rendered. Misses are cached.
  - `PackageRow` and `RepoRow` show the icon when `icon.width > 0`, else the existing avatar.
- Flatpak and AppImage rows are unchanged.

## B. Flatpak detail page

### Data

- AppStream parsing moves out of `main.rs` into `crates/xpm-ui/src/appstream.rs`, behaviour-preserving for all existing fields (verified by running old and new parsers over the same Flathub data and diffing the JSON).
- New fields on `CachedRemoteApp` (all `#[serde(default)]`): `screenshots: Vec<Screenshot { url, source_url, caption }>`, `license: String`, `releases: Vec<Release { version, date, notes }>` (newest 5), `url_donation`, `url_help`, `url_faq`.
- Cache file is renamed (`remote_<name>.v2.json`) so older caches are rebuilt once.

### Runtime

- Screenshots download lazily in background threads to `~/.cache/xpm/flatpak-screenshots/<app-id>/<n>.png` (cached, 20 s timeout) and arrive as `FlatpakScreenshotReady { index, path }`. Images are built on the UI thread.
- Extras (flathub remote only): one background thread, `curl --max-time 8` against `/api/v2/summary/<id>`, `/api/v2/verification/<id>/status`, `/api/v2/stats/<id>`; result `FlatpakExtrasReady { download_size, installed_size, verified, installs_total }`. All parsed with `serde_json`; every failure is silent.

### Layout (all cards rounded, themed from `Cat`)

1. Header card: 96 px icon, name, summary, developer (verified badge when true), category pills, Install/Remove and Add-Ons buttons (existing callbacks and modal).
2. Screenshot carousel card: rounded clip, previous/next arrows, dot indicators, caption; hidden when no screenshots.
3. Description card.
4. Information grid of rounded cards: Version + release date, License, Download size, Installed size, Installs, App ID, Remote.
5. Release notes card: latest release expanded, up to four earlier ones listed.
6. Links card: homepage, bug tracker, translate, source, donate, help.

## Error handling

- Missing icon, theme file, catalog, screenshot or API response means that element is omitted; nothing blocks rendering.
- Malformed `.desktop` files, AppStream components and API JSON are skipped, not fatal.

## Testing

- Unit tests: `.desktop` parsing and icon resolution against a temp directory tree, catalog XML mapping, AppStream multi-screenshot and multi-release parsing, Flathub JSON parsing.
- Parity check (throwaway) of the moved parser against the previous output on the local Flathub AppStream data.
- Screenshots of: Installed with icons, Search with icons, Flatpak detail page (app with and without screenshots).

## Out of scope

Icons for Flatpak/AppImage rows (already present), editing the Add-Ons modal, non-Flathub remote API enrichment.
