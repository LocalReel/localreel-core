#!/usr/bin/env bash
# User-local Linux installer for LocalReel.
set -euo pipefail

SOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR="${LOCALREEL_HOME:-$HOME/.local/share/localreel}"
START_ON_LOGIN=0
NO_DESKTOP=0

usage() {
  cat <<'EOF'
Usage: ./install.sh [options]

  --install-dir <dir>  install directory (default: ~/.local/share/localreel)
  --start-on-login     also create ~/.config/autostart/localreel.desktop
  --no-desktop         do not create an application menu entry
  -h, --help           show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --install-dir) INSTALL_DIR="${2:?missing install dir}"; shift 2 ;;
    --start-on-login) START_ON_LOGIN=1; shift ;;
    --no-desktop) NO_DESKTOP=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [ ! -x "$SOURCE_DIR/localreel" ]; then
  echo "missing executable: $SOURCE_DIR/localreel" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
for item in "$SOURCE_DIR"/*; do
  [ -e "$item" ] || continue
  base="$(basename "$item")"
  case "$base" in
    install.sh) continue ;;
  esac
  cp -R "$item" "$INSTALL_DIR"/
done
chmod 0755 "$INSTALL_DIR/localreel"
if [ ! -f "$INSTALL_DIR/localreel.toml" ] && [ -f "$INSTALL_DIR/localreel.toml.example" ]; then
  cp "$INSTALL_DIR/localreel.toml.example" "$INSTALL_DIR/localreel.toml"
fi

if [ "$NO_DESKTOP" -eq 0 ]; then
  DESKTOP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
  AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
  mkdir -p "$DESKTOP_DIR"
  DESKTOP_FILE="$DESKTOP_DIR/localreel.desktop"
  cat > "$DESKTOP_FILE" <<EOF
[Desktop Entry]
Type=Application
Name=LocalReel
Comment=Local-first media server
Exec="$INSTALL_DIR/localreel" serve
Path=$INSTALL_DIR
Terminal=true
Categories=AudioVideo;Video;
EOF
  chmod 0644 "$DESKTOP_FILE"
  echo "Desktop entry: $DESKTOP_FILE"

  if [ "$START_ON_LOGIN" -eq 1 ]; then
    mkdir -p "$AUTOSTART_DIR"
    cp "$DESKTOP_FILE" "$AUTOSTART_DIR/localreel.desktop"
    echo "Autostart: $AUTOSTART_DIR/localreel.desktop"
  fi
fi

echo "Installed: $INSTALL_DIR"
echo "Start: $INSTALL_DIR/localreel serve"
echo "Open:  http://127.0.0.1:3001"
