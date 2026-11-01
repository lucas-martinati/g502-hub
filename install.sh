#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd "$root"
cargo build --locked --release
data="${XDG_DATA_HOME:-$HOME/.local/share}/g502-hub-rust"
menu="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$data/bin" "$menu"
for binary in g502-hub g502-hubd; do
    install -m755 "target/release/$binary" "$data/bin/$binary.new"
    mv "$data/bin/$binary.new" "$data/bin/$binary"
done
cp LICENSE NOTICE.md README.md "$data/"
cat > "$menu/io.github.lucas_martinati.G502Hub.desktop" <<ENTRY
[Desktop Entry]
Type=Application
Name=G502 Hub
Comment=DPI, lighting, buttons and profiles for the Logitech G502 HERO
Exec="$data/bin/g502-hub"
Icon=input-mouse
Terminal=false
Categories=Settings;HardwareSettings;
StartupWMClass=g502-hub
ENTRY
printf 'Installed G502 Hub. Open it from the application menu.\n'
