#!/usr/bin/env bash
# User-local Linux uninstaller. Keeps data/config unless --remove-data.
set -euo pipefail

INSTALL_DIR="${LOCALREEL_HOME:-}"
REMOVE_DATA=0
ASSUME_YES=0
STOP_SERVER=0

usage() {
  cat <<'EOF'
Usage: ./uninstall.sh [options]

  --install-dir <dir>  install directory (default: ~/.local/share/localreel)
  --remove-data        delete data/ and localreel.toml too
  --stop-server        stop a process whose command line points at this install
  --yes                do not ask for confirmation
  -h, --help           show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --install-dir) INSTALL_DIR="${2:?missing install dir}"; shift 2 ;;
    --remove-data) REMOVE_DATA=1; shift ;;
    --stop-server) STOP_SERVER=1; shift ;;
    --yes|-y) ASSUME_YES=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [ -z "$INSTALL_DIR" ]; then
  script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  if [ -x "$script_dir/localreel" ]; then
    INSTALL_DIR="$script_dir"
  else
    INSTALL_DIR="$HOME/.local/share/localreel"
  fi
fi

if [ ! -d "$INSTALL_DIR" ]; then
  echo "not installed: $INSTALL_DIR"
  exit 0
fi

if [ "$STOP_SERVER" -eq 1 ] && command -v pkill >/dev/null 2>&1; then
  pkill -f "$INSTALL_DIR/localreel" >/dev/null 2>&1 || true
  sleep 1
fi

if [ "$ASSUME_YES" -eq 0 ]; then
  if [ "$REMOVE_DATA" -eq 1 ]; then
    detail="data and progress will be deleted"
  else
    detail="data and localreel.toml will be kept"
  fi
  printf 'Uninstall LocalReel from %s? %s [y/N] ' "$INSTALL_DIR" "$detail"
  read -r answer
  case "$answer" in
    y|Y|yes|YES) ;;
    *) echo "Canceled."; exit 0 ;;
  esac
fi

DESKTOP_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
AUTOSTART_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
rm -f "$DESKTOP_DIR/localreel.desktop"
rm -f "$AUTOSTART_DIR/localreel.desktop"

if [ "$REMOVE_DATA" -eq 1 ]; then
  rm -rf "$INSTALL_DIR"
  echo "Removed $INSTALL_DIR (including data)."
else
  find "$INSTALL_DIR" -mindepth 1 -maxdepth 1 \
    ! -name data ! -name localreel.toml -exec rm -rf {} +
  echo "Removed program files from $INSTALL_DIR."
  [ -d "$INSTALL_DIR/data" ] && echo "Kept data: $INSTALL_DIR/data"
  [ -f "$INSTALL_DIR/localreel.toml" ] && echo "Kept config: $INSTALL_DIR/localreel.toml"
fi
