#!/usr/bin/env bash
set -euo pipefail

# Build every target the current host can produce, write one ZIP per OS/arch
# into dist/, and publish a GitHub release once all six archives exist.
# The tag is always v<version in Cargo.toml> and cannot be overridden.

usage() {
  cat <<'EOF'
Usage: scripts/release.sh [OPTIONS]

Options:
  --only LABEL     Build one target: linux-x64, linux-arm64, macos-x64,
                   macos-arm64, windows-x64 or windows-arm64.
  --no-push        Build and ZIP only; never publish.
  --push-only      Skip builds; publish the existing dist/*.zip files.
  -h, --help       Show this help.

The release tag is derived from the version in Cargo.toml (v<version>).
Bump Cargo.toml and commit before starting a new release.
EOF
}

die() { printf 'error: %s\n' "$*" >&2; exit 1; }
warn() { printf 'warning: %s\n' "$*" >&2; }
info() { printf '==> %s\n' "$*"; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ONLY=""
NO_PUSH=0
PUSH_ONLY=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --only)
      [[ $# -ge 2 ]] || die "--only requires a label"
      ONLY="$2"
      shift 2
      ;;
    --no-push) NO_PUSH=1; shift ;;
    --push-only) PUSH_ONLY=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
done
[[ $NO_PUSH -eq 0 || $PUSH_ONLY -eq 0 ]] || die "--no-push and --push-only are mutually exclusive"

VERSION="$(grep -m1 '^version[[:space:]]*=' Cargo.toml | sed -E 's/^version[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/')"
[[ -n "$VERSION" ]] || die "could not read version from Cargo.toml"
TAG="v${VERSION}"

HOST_OS="$(uname -s)"
HOST_ARCH="$(uname -m)"

TARGETS=(
  "linux-x64:x86_64-unknown-linux-gnu:mesh-chat"
  "linux-arm64:aarch64-unknown-linux-gnu:mesh-chat"
  "macos-x64:x86_64-apple-darwin:mesh-chat"
  "macos-arm64:aarch64-apple-darwin:mesh-chat"
  "windows-x64:x86_64-pc-windows-msvc:mesh-chat.exe"
  "windows-arm64:aarch64-pc-windows-msvc:mesh-chat.exe"
)

host_name() {
  case "$1" in
    linux-*) printf 'Linux' ;;
    macos-*) printf 'macOS' ;;
    windows-*) printf 'Windows' ;;
  esac
}

host_supports() {
  case "$1" in
    linux-*) [[ "$HOST_OS" == Linux ]] ;;
    macos-*) [[ "$HOST_OS" == Darwin ]] ;;
    windows-*) [[ "$HOST_OS" == MINGW* || "$HOST_OS" == MSYS* || "$HOST_OS" == CYGWIN* ]] ;;
    *) return 1 ;;
  esac
}

zip_path() { printf '%s/dist/mesh-chat-%s-%s.zip' "$ROOT" "$TAG" "$1"; }

install_std() {
  local triple="$1"
  if command -v rustup >/dev/null 2>&1; then
    if ! rustup target add "$triple"; then
      warn "rustup could not install std for $triple; skipping"
      return 1
    fi
  fi
  return 0
}

cross_cc() {
  # Print the cross compiler needed for a label on this host, if any.
  case "$1:$HOST_ARCH" in
    linux-x64:aarch64|linux-x64:arm64) printf 'x86_64-linux-gnu-gcc' ;;
    linux-arm64:x86_64|linux-arm64:amd64) printf 'aarch64-linux-gnu-gcc' ;;
  esac
}

build_target() {
  local label="$1" triple="$2" bin="$3"
  local cc_var="CC_${triple//-/_}"
  local cc
  cc="$(cross_cc "$label")"

  if [[ -n "$cc" ]] && ! command -v "$cc" >/dev/null 2>&1; then
    warn "$label needs $cc (install the cross toolchain); skipping"
    return 0
  fi
  install_std "$triple" || return 0
  command -v zip >/dev/null 2>&1 || die "zip is required to create release archives"

  info "building $label ($triple)"
  if [[ -n "$cc" ]]; then
    if ! env "${cc_var}=${cc}" cargo build --release --locked --target "$triple"; then
      warn "cargo build failed for $label; skipping"
      return 0
    fi
  elif ! cargo build --release --locked --target "$triple"; then
    warn "cargo build failed for $label; skipping"
    return 0
  fi

  local built="$ROOT/target/$triple/release/$bin"
  [[ -f "$built" ]] || { warn "expected binary $built not found; skipping"; return 0; }

  local staging
  staging="$(mktemp -d)"
  cp "$built" "$staging/$bin"
  cp "$ROOT/README.md" "$staging/README.md"
  mkdir -p "$ROOT/dist"
  (cd "$staging" && zip -q -r "$(zip_path "$label")" .)
  rm -rf "$staging"
  info "created $(zip_path "$label")"
}

push_release() {
  command -v gh >/dev/null 2>&1 || die "GitHub CLI (gh) is required to publish"

  local -a archives=()
  local spec label zipfile
  for spec in "${TARGETS[@]}"; do
    IFS=: read -r label _ <<<"$spec"
    zipfile="$(zip_path "$label")"
    [[ -f "$zipfile" ]] || die "missing $zipfile; run this script on the remaining hosts first"
    archives+=("$zipfile")
  done

  if command -v git >/dev/null 2>&1 && git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    die "tag $TAG already exists locally; bump the version in Cargo.toml"
  fi
  if gh release view "$TAG" >/dev/null 2>&1; then
    die "release $TAG already exists; bump the version in Cargo.toml"
  fi

  info "publishing GitHub release $TAG"
  gh release create "$TAG" "${archives[@]}" \
    --title "$TAG" \
    --notes "mesh-chat $TAG - binaries for Linux, macOS and Windows (x64 and ARM64)."
}

if [[ $PUSH_ONLY -eq 1 ]]; then
  push_release
  exit 0
fi

for spec in "${TARGETS[@]}"; do
  IFS=: read -r label triple bin <<<"$spec"
  if [[ -n "$ONLY" && "$label" != "$ONLY" ]]; then
    continue
  fi
  if ! host_supports "$label"; then
    if [[ -n "$ONLY" ]]; then
      die "$label can only be built on $(host_name "$label"); run this on a supported host"
    fi
    warn "skipping $label (requires a $(host_name "$label") host)"
    continue
  fi
  build_target "$label" "$triple" "$bin"
done

if [[ $NO_PUSH -eq 1 ]]; then
  info "build finished; archives are in $ROOT/dist"
  exit 0
fi

missing=0
for spec in "${TARGETS[@]}"; do
  IFS=: read -r label _ <<<"$spec"
  if [[ ! -f "$(zip_path "$label")" ]]; then
    warn "missing $(zip_path "$label")"
    missing=$((missing + 1))
  fi
done
if [[ $missing -gt 0 ]]; then
  info "$missing archive(s) missing. Run this script on the remaining host OSes, then use --push-only."
  exit 0
fi
push_release
