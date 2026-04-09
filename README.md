# xPackageManager

A modern package manager for Arch Linux supporting pacman (via libalpm) and Flatpak backends.

<img width="1128" height="774" alt="image" src="Screenshot.png" />

## Features

- **Dual Backend Support**: Manage both pacman packages and Flatpak applications from a single interface
- **Modern Qt 6 UI**: Built with QML and Qt Quick Controls 2 for a native desktop experience
- **Rust Backend**: Safe, fast, and concurrent package management operations
- **System Maintenance**: Orphan detection, cache cleanup, and database synchronization

## Building

### Prerequisites

- Rust (Edition 2024)
- Qt 6.2+ with QtQuick and QtQuickControls2
- libalpm (pacman library)
- flatpak development libraries
- CMake 3.24+ (optional, for CMake integration)

### Arch Linux Dependencies

```bash
sudo pacman -S rust qt6-base qt6-declarative qt6-quickcontrols2 pacman flatpak
```

### Build with Cargo

```bash
cargo build --release
```

### Build with CMake (optional)

```bash
cmake -B build
cmake --build build
```

## Running

```bash
# Development
cargo run --bin xpm-ui

# Release
./target/release/xpm-ui
```

## Architecture

### Core Types (`xpm-core`)

- `Package`: Package metadata (name, version, description, backend)
- `Operation`: Install/remove/update operations
- `PackageSource`: Trait for backend implementations

### Pacman Backend (`xpm-alpm`)

- Read-only operations (search, list, info) work without privileges
- Write operations (install, remove, upgrade) require root via polkit

### Flatpak Backend (`xpm-flatpak`)

- User-level installations work without root
- System-level installations require appropriate permissions

### Service Layer (`xpm-service`)

- `PackageManager`: Orchestrates multiple backends
- Progress tracking for long-running operations
- Application state management

### UI Layer (`xpm-ui`)

- CXX-Qt bridges expose Rust logic to QML
- QML views for different package management tasks
- Native Qt theming support

## TODO

- [ ] Ability to `downgrade` pkgs.
- [ ] System update notifications.
- [ ] Dependency tree visualization.

## License

GPL-3.0-or-later
