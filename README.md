# xPackageManager

A modern, dual-backend package manager for Arch Linux — manages both **pacman** (via libalpm) and **Flatpak** applications from a single interface, built with Rust and the [Slint](https://slint.dev) UI framework.

<img width="1128" height="774" alt="image" src="Screenshot.png" />

---

## Features

### Package Management

- **Installed Packages** — browse, search, and remove all installed pacman packages with live dependency info
- **Available Updates** — unified updates view showing both native and Flatpak updates with per-format update buttons
- **Package Search** — search the full pacman sync databases in real time
- **Local Install** — install local `.pkg.tar.zst` files via file picker
- **Dependency Tree** — visualise full dependency and reverse-dependency graphs for any package

### Flatpak Browser

- **App Store view** — browse the full Flathub catalogue with category filters and search
- **App Detail page** — icon, screenshot banner, formatted description, changelog card, links section (homepage / bug tracker / translations / source), and category tags
- **Add-Ons modal** — per-app add-ons split into "Available to Install" and "Already Installed" sections with individual Remove buttons
- **Installed Flatpaks** — separate tab listing installed Flatpak apps with remove support

### Updates

- **Mixed updates list** — native pacman and Flatpak updates shown together in the Updates view
- **Separate update actions**:
  - Updates view: "Update Flatpaks (N)" and "Update All" (native) buttons
  - Flatpaks view: "Update Flatpaks (N)" button
  - Browse Repos view: "Update Native (N)" button
- **Plasmoid/widget updates** also detected and listed

### System & Maintenance

- **Home Dashboard** — system stats (CPU, RAM, disk, GPU, kernel, uptime), quick-action tiles, Arch Linux RSS news feed
- **Settings** — toggle flatpak support, auto-update checks, parallel downloads, cache retention
- **Browse Repos** — navigate pacman repositories and browse packages per repo
- **System operations** — mirror list update, keyring fix, initramfs rebuild, GRUB rebuild

### Terminal & Operations

- **Live terminal output** — VT100-aware progress popup with auto-scroll; in-place progress bars render correctly (no duplicate lines)
- **Conflict resolution dialog** — handles pacman file conflicts and dependency breaks with force-install option
- **User prompt detection** — surfaces interactive pacman prompts (provider selection, key import) to the user in-app
- **Operation cancellation** — SIGTERM support for all running operations

---

## Architecture

### Crate Layout

```
xPackageManager/
├── crates/
│   ├── xpm-core/       # Shared types: Package, Operation, PackageSource trait
│   ├── xpm-alpm/       # Pacman backend via libalpm
│   ├── xpm-flatpak/    # Flatpak backend (list, install, remove, updates)
│   ├── xpm-service/    # Orchestration, progress tracking, state management
│   └── xpm-ui/
│       ├── src/main.rs # Rust logic, backend threads, UI message loop
│       └── ui/main.slint # Slint declarative UI
```

### UI Architecture

- **Slint** declarative UI — single `main.slint` file, Catppuccin-inspired palette derived from the system theme
- **Message passing** — background threads communicate with the main thread via `mpsc::channel<UiMessage>`, polled on a 50 ms timer
- **Flatpak appstream** — parses local appstream XML (or `.gz`) to build the app catalogue; cached to `~/.local/share/xpm/remote_flathub.json` (24 h TTL)

### Pacman Backend (`xpm-alpm`)

- Read operations (search, list, info) work without privileges
- Write operations (install, remove, upgrade) run via PTY with automatic `sudo` escalation

### Flatpak Backend (`xpm-flatpak`)

- User-level installs work without root
- Appstream XML parsed locally — no network calls for browsing

---

## Building

### Dependencies

```bash
sudo pacman -S rust cargo flatpak alpm
```

### Run

```bash
git clone https://github.com/xerolinux/xPackageManager
cd xPackageManager
cargo run --release --bin xpackagemanager
```

### Install (XeroLinux repo)

```bash
echo -e '\n[xerolinux]\nSigLevel = Optional TrustAll\nServer = https://repos.xerolinux.xyz/$repo/$arch' | sudo tee -a /etc/pacman.conf
sudo pacman -Syy coming-soon-lol
```

---

## License

GPL-3.0-or-later
