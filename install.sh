#!/bin/sh
set -eu

REPO="lucas-martinati/g502-hub"

if [ "${EUID:-$(id -u)}" -eq 0 ]; then
    printf 'Error: Do not run install.sh as root (sudo). G502 Hub installs per-user.\n' >&2
    exit 1
fi

TMP_WORK=""
cleanup() {
    if [ -n "$TMP_WORK" ] && [ -d "$TMP_WORK" ]; then
        rm -rf "$TMP_WORK"
    fi
}
trap cleanup EXIT

SCRIPT_DIR=""
case "$0" in
    *install.sh)
        if [ -f "$0" ]; then
            SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
        fi
        ;;
esac

SRC_DIR=""
BIN_DIR=""

if [ -n "$SCRIPT_DIR" ] && [ -x "$SCRIPT_DIR/g502-hub" ] && [ -x "$SCRIPT_DIR/g502-hubd" ]; then
    SRC_DIR="$SCRIPT_DIR"
    BIN_DIR="$SCRIPT_DIR"
elif [ -n "$SCRIPT_DIR" ] && [ -f "$SCRIPT_DIR/Cargo.toml" ]; then
    cd "$SCRIPT_DIR"
    cargo build --locked --release
    SRC_DIR="$SCRIPT_DIR"
    BIN_DIR="$SCRIPT_DIR/target/release"
else
    case "$(uname -m)" in
        aarch64|arm64) ARCH="linux-aarch64" ;;
        *)             ARCH="linux-x86_64" ;;
    esac
    printf 'Downloading latest G502 Hub release (%s)...\n' "$ARCH"
    TMP_WORK=$(mktemp -d "${TMPDIR:-/tmp}/g502-hub-install-XXXXXX")
    BASE_URL="https://github.com/$REPO/releases/latest/download"
    ARCHIVE="g502-hub-$ARCH.tar.gz"
    curl -fsSL "$BASE_URL/$ARCHIVE" -o "$TMP_WORK/$ARCHIVE"
    if curl -fsSL "$BASE_URL/$ARCHIVE.sha256" -o "$TMP_WORK/$ARCHIVE.sha256" 2>/dev/null; then
        if command -v sha256sum >/dev/null 2>&1; then
            EXPECTED=$(awk '{print $1}' "$TMP_WORK/$ARCHIVE.sha256")
            ACTUAL=$(sha256sum "$TMP_WORK/$ARCHIVE" | awk '{print $1}')
            if [ "$EXPECTED" != "$ACTUAL" ]; then
                printf 'Error: SHA-256 checksum mismatch for %s\n' "$ARCHIVE" >&2
                exit 1
            fi
        fi
    fi
    mkdir -p "$TMP_WORK/pkg"
    tar -xzf "$TMP_WORK/$ARCHIVE" --strip-components=1 -C "$TMP_WORK/pkg"
    SRC_DIR="$TMP_WORK/pkg"
    BIN_DIR="$TMP_WORK/pkg"
fi

data="${XDG_DATA_HOME:-$HOME/.local/share}/g502-hub-rust"
config_root="${XDG_CONFIG_HOME:-$HOME/.config}"
menu="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
local_bin="$HOME/.local/bin"
autostart="$config_root/autostart/io.github.lucas_martinati.G502Hub.desktop"
legacy_autostart="$config_root/autostart/g502-hub.desktop"
desktop="$menu/io.github.lucas_martinati.G502Hub.desktop"
legacy_desktop="$menu/g502-hub.desktop"

was_running=0
if [ -x "$data/bin/g502-hubd" ] && "$data/bin/g502-hubd" --status >/dev/null 2>&1; then
    was_running=1
    "$data/bin/g502-hubd" --stop >/dev/null 2>&1 || true
fi

mkdir -p "$data/bin" "$data/backups" "$menu" "$local_bin"
for binary in g502-hub g502-hubd; do
    install -m755 "$BIN_DIR/$binary" "$data/bin/$binary.new"
    mv "$data/bin/$binary.new" "$data/bin/$binary"
    if [ ! -e "$local_bin/$binary" ] || [ -L "$local_bin/$binary" ]; then
        ln -sfn "$data/bin/$binary" "$local_bin/$binary"
    fi
done

for doc in LICENSE NOTICE.md README.md; do
    if [ -f "$SRC_DIR/$doc" ]; then
        cp "$SRC_DIR/$doc" "$data/"
    fi
done

if [ -f "$desktop" ]; then
    cp "$desktop" "$data/backups/io.github.lucas_martinati.G502Hub.desktop.bak"
fi
escaped_gui=$(printf '%s' "$data/bin/g502-hub" | sed 's/\\/\\\\/g; s/"/\\"/g')
cat > "$desktop" <<ENTRY
[Desktop Entry]
Type=Application
Name=G502 Hub
Comment=DPI, lighting, buttons and profiles for the Logitech G502 HERO
Exec="$escaped_gui"
Icon=input-mouse
Terminal=false
Categories=Settings;HardwareSettings;
StartupWMClass=g502-hub
ENTRY

if [ -f "$legacy_desktop" ] && grep -q "g502-hub-rust/bin/g502-hub" "$legacy_desktop"; then
    rm -f "$legacy_desktop"
fi

autostart_enabled=0
if [ -f "$autostart" ]; then
    autostart_enabled=1
fi
if [ -f "$legacy_autostart" ] && grep -q "g502-hub-rust/bin/g502-hub" "$legacy_autostart"; then
    autostart_enabled=1
    rm -f "$legacy_autostart"
fi
if [ "$autostart_enabled" -eq 1 ]; then
    mkdir -p "$config_root/autostart"
    cat > "$autostart" <<AUTO
[Desktop Entry]
Type=Application
Name=G502 Hub
Exec="$escaped_gui" --background
Icon=input-mouse
Terminal=false
X-GNOME-Autostart-enabled=true
AUTO
fi

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$menu" >/dev/null 2>&1 || true
fi

if [ "$was_running" -eq 1 ]; then
    "$data/bin/g502-hub" --background >/dev/null 2>&1 || true
fi

printf 'Installed G502 Hub (%s). Open it from the application menu or run g502-hub.\n' "$("$data/bin/g502-hub" --version)"
