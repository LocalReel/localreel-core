#!/usr/bin/env bash
# Build a Linux tarball. Run this on Linux; the `--allow-non-linux` flag is
# only for structural tests of the script (it does not make the binary run).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WEB_DIR="$(cd "$ROOT/../localreel-web" && pwd)"
VERSION=""
OUT_DIR="$ROOT/dist"
SKIP_BUILD=0
BUILD_WEB=0
ALLOW_NON_LINUX=0
FFMPEG_DIR=""

usage() {
  cat <<'EOF'
Usage: scripts/package-linux.sh [options]

  --version <v>       override version (default: workspace Cargo.toml)
  --out <dir>         output directory (default: core/dist)
  --skip-build        package target/release/localreel as-is
  --build-web         run npm run build in ../localreel-web first
  --ffmpeg-dir <dir>  copy ffmpeg/ffprobe into ffmpeg/bin
  --allow-non-linux   structural test only; skips the Linux platform check
  -h, --help          show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?missing version}"; shift 2 ;;
    --out) OUT_DIR="${2:?missing output dir}"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --build-web) BUILD_WEB=1; shift ;;
    --ffmpeg-dir) FFMPEG_DIR="${2:?missing ffmpeg dir}"; shift 2 ;;
    --allow-non-linux) ALLOW_NON_LINUX=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

SYSTEM="$(uname -s)"
if [ "$SYSTEM" != "Linux" ] && [ "$ALLOW_NON_LINUX" -eq 0 ]; then
  echo "package-linux.sh must run on Linux (detected: $SYSTEM)" >&2
  echo "use --allow-non-linux only for structural tests" >&2
  exit 1
fi

if [ -z "$VERSION" ]; then
  VERSION="$(awk '
    /^\[workspace.package\]/ { in_section=1; next }
    /^\[/ { in_section=0 }
    in_section && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
  ' "$ROOT/Cargo.toml")"
fi
if [ -z "$VERSION" ]; then
  echo "could not determine version" >&2
  exit 1
fi

MACHINE="$(uname -m)"
case "$MACHINE" in
  x86_64|amd64) ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *) ARCH="$MACHINE" ;;
esac

if [ "$BUILD_WEB" -eq 1 ]; then
  echo "building web assets..."
  (cd "$WEB_DIR" && npm run build)
fi

if [ ! -f "$WEB_DIR/dist/index.html" ]; then
  echo "missing $WEB_DIR/dist/index.html; build the web UI first or pass --build-web" >&2
  exit 1
fi

if [ "$SKIP_BUILD" -eq 0 ]; then
  echo "building localreel-server (release)..."
  (cd "$ROOT" && cargo build --release -p localreel-server)
fi

BINARY="$ROOT/target/release/localreel"
if [ ! -f "$BINARY" ]; then
  echo "missing $BINARY (run without --skip-build)" >&2
  exit 1
fi

STAGE_NAME="localreel-$VERSION-$ARCH"
STAGE="$OUT_DIR/$STAGE_NAME"
mkdir -p "$OUT_DIR"
rm -rf "$STAGE"
mkdir -p "$STAGE/ffmpeg" "$STAGE/docs" "$STAGE/tools"

cat "$BINARY" > "$STAGE/localreel"
chmod 0755 "$STAGE/localreel"
install -m 0644 "$ROOT/localreel.toml.example" "$STAGE/localreel.toml.example"
install -m 0644 "$ROOT/README.md" "$STAGE/README.md"
install -m 0755 "$ROOT/scripts/install.sh" "$STAGE/install.sh"
install -m 0755 "$ROOT/scripts/uninstall.sh" "$STAGE/uninstall.sh"
for doc in operations.md packaging.md install.md release-checklist.md; do
  install -m 0644 "$ROOT/docs/$doc" "$STAGE/docs/$doc"
done
install -m 0755 "$ROOT/scripts/fetch-ffmpeg.ps1" "$STAGE/tools/fetch-ffmpeg.ps1" || true
install -m 0755 "$ROOT/scripts/verify-ffmpeg.ps1" "$STAGE/tools/verify-ffmpeg.ps1" || true

if [ -n "$FFMPEG_DIR" ]; then
  if [ -x "$FFMPEG_DIR/ffmpeg" ] && [ -x "$FFMPEG_DIR/ffprobe" ]; then
    install -m 0755 "$FFMPEG_DIR/ffmpeg" "$STAGE/ffmpeg/bin/ffmpeg"
    install -m 0755 "$FFMPEG_DIR/ffprobe" "$STAGE/ffmpeg/bin/ffprobe"
  elif [ -x "$FFMPEG_DIR/bin/ffmpeg" ] && [ -x "$FFMPEG_DIR/bin/ffprobe" ]; then
    install -m 0755 "$FFMPEG_DIR/bin/ffmpeg" "$STAGE/ffmpeg/bin/ffmpeg"
    install -m 0755 "$FFMPEG_DIR/bin/ffprobe" "$STAGE/ffmpeg/bin/ffprobe"
  else
    echo "ffmpeg/ffprobe not found in $FFMPEG_DIR" >&2
    exit 1
  fi
fi

cat > "$STAGE/README.txt" <<EOF
LocalReel $VERSION ($ARCH)

Quick start (Linux):
  1. Optional: ./install.sh   (copies to ~/.local/share/localreel + desktop entry)
  2. Or run in place: cp localreel.toml.example localreel.toml, edit it,
     then ./localreel doctor && ./localreel serve.
  3. Optional: put ffmpeg/ffprobe in ffmpeg/bin or install ffmpeg from your distro.
  4. Open http://127.0.0.1:3001

The web UI is embedded in the binary; no separate dist directory is needed.
EOF

ARCHIVE="$OUT_DIR/$STAGE_NAME.tar.gz"
rm -f "$ARCHIVE"
tar -C "$OUT_DIR" -czf "$ARCHIVE" "$STAGE_NAME"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$OUT_DIR" && sha256sum "$STAGE_NAME.tar.gz" > "$STAGE_NAME.tar.gz.sha256")
  echo "SHA256: $(cut -d' ' -f1 "$ARCHIVE.sha256")"
fi
echo "Package: $ARCHIVE"
du -h "$ARCHIVE" | awk '{print "Size: " $1}'
