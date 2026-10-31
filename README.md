# xPackageManager

A package manager GUI for Arch Linux that manages **pacman**, **Flatpak** and **AppImage** from one interface. Built with Rust and [Slint](https://slint.dev).

Included by default on XeroLinux.

<video src="https://github.com/xerolinux/xPackageManager/raw/main/media/xpm-demo.mp4" poster="https://github.com/xerolinux/xPackageManager/raw/main/media/xpm-demo.png" controls width="900">
  <a href="media/xpm-demo.mp4">Watch the demo video</a>
</video>

---

## Install on other Arch-based distros

Requires `base-devel` and `git`:

```bash
sudo pacman -S --needed base-devel git
git clone https://github.com/xerolinux/xPackageManager
cd xPackageManager/packaging && ./build.sh
```

To update, pull and rebuild:

```bash
cd xPackageManager && git pull
cd packaging && ./build.sh
```

---

## Changelog

### v0.8.1

- Update popup split into one page per package type, with a result card for each.
- Startup sound (toggle in Settings) and a one-time welcome notice.
- Opening an AppImage offers Run or Install, with a right-click action in Dolphin.

### v0.8.0

- Update All with one confirmation for native, Flatpak and AppImage.
- New progress dialog with per-package progress.
- One-click Flatpak and AppImage installs.
- Open `.AppImage`, `.flatpak`, `.flatpakref` and `.flatpakrepo` files with xpm.

### v0.7.0

- One progress dialog for pacman, Flatpak and AppImage, with Yes/No confirmations.
- Update All now updates everything that is enabled in one run.
- App icons for native, Flatpak and AppImage packages.
- Redesigned Flatpak info page with screenshots, details and release notes.
- Faster, smoother interface and a cleaner look.

### v0.6.x

- AppImage support, Flatpak permission editor and pacman rollback.

### v0.5.x

- Translations and more settings (HoldPkg, cache auto-clean, notifications, Flatpak remotes).

### v0.4.0

- Flatpak details with add-ons, dependency tree and activity log.
