#!/usr/bin/env bash
# build.sh — build arcanum into the local plugin tree, the same way the release
# workflow packages it.
#
# Compiles the CLI (debug by default, --release for a release build) and zips
# the bare binary as the release-named cli_zip — the exact asset name from
# objectiveai.json's `cli_zip` map — then drops it, UNEXTRACTED, into the plugin
# tree under .objectiveai, with the manifest at the version-dir head:
#
#   .objectiveai/bin/plugins/<owner>/<name>/<version>/
#     objectiveai.json                       ← the manifest
#     cli/<name>-<os>-<arch>.zip             ← the cli_zip (the bare binary)
#
# This mirrors what the host's `plugins install` lays down, so a test harness
# (or the host) can unpack the cli_zip in place. Coords + version come from
# objectiveai.json.
#
# Also installs the objectiveai host binaries into .objectiveai/bin (for the
# integration tests), via the upstream installer pinned to our objectiveai-sdk
# version tag — it caches the versioned release zip under .objectiveai/bin and
# reuses it on re-runs. Skip that with --no-test (e.g. the release build, which
# only needs to package the plugin).
#
# Usage:
#   bash build.sh                       # debug,   + objectiveai host
#   bash build.sh --release             # release, + objectiveai host
#   bash build.sh --release --no-test   # release, plugin only (no host)
set -euo pipefail

REL=""
PROFILE="debug"
NO_TEST=0
for arg in "$@"; do
  case "$arg" in
    --release) REL="--release"; PROFILE="release" ;;
    --no-test) NO_TEST=1 ;;
    *) echo "build.sh: unknown arg: $arg (usage: build.sh [--release] [--no-test])" >&2; exit 1 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

# ── platform / arch (drives the cli_zip name + the binary's extension) ──────
case "$(uname -s)" in
  Linux*)               PLATFORM="linux"   ;;
  Darwin*)              PLATFORM="macos"   ;;
  CYGWIN*|MINGW*|MSYS*) PLATFORM="windows" ;;
  *) echo "build.sh: unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  ARCH="x86_64"  ;;
  arm64|aarch64) ARCH="aarch64" ;;
  *) echo "build.sh: unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac
if [ "$PLATFORM" = "windows" ]; then EXE=".exe"; else EXE=""; fi

# ── plugin coords (single source of truth: objectiveai.json) ───────────────
OWNER="$(sed -n -E 's/.*"owner"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' objectiveai.json | head -1)"
NAME="$(sed -n -E 's/.*"name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' objectiveai.json | head -1)"
VERSION="$(sed -n -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' objectiveai.json | head -1)"
[ -n "$OWNER" ] && [ -n "$NAME" ] && [ -n "$VERSION" ] \
  || { echo "build.sh: could not read owner/name/version from objectiveai.json" >&2; exit 1; }

# ── build (same as the releaser: cargo build [--release]) ──────────────────
echo "==> build.sh ($PROFILE): cargo build${REL:+ $REL}"
cargo build $REL
CLI_BIN="$REPO_ROOT/target/$PROFILE/$NAME$EXE"
[ -f "$CLI_BIN" ] || { echo "build.sh: built binary missing at $CLI_BIN" >&2; exit 1; }

# ── stage into the plugin tree under .objectiveai ──────────────────────────
PLUGIN_DIR="$REPO_ROOT/.objectiveai/bin/plugins/$OWNER/$NAME/$VERSION"
CLI_DIR="$PLUGIN_DIR/cli"
CLI_ZIP="$CLI_DIR/$NAME-$PLATFORM-$ARCH.zip"

mkdir -p "$PLUGIN_DIR"
# Manifest sits at the head, above cli/.
cp "$REPO_ROOT/objectiveai.json" "$PLUGIN_DIR/objectiveai.json"

# cli_zip = the bare binary, flat at the zip root (matches the release asset).
rm -rf "$CLI_DIR"; mkdir -p "$CLI_DIR"
case "$PLATFORM" in
  windows)
    powershell.exe -NoProfile -Command \
      "Compress-Archive -Path '$(cygpath -w "$CLI_BIN")' -DestinationPath '$(cygpath -w "$CLI_ZIP")' -Force"
    ;;
  *)
    zip -j "$CLI_ZIP" "$CLI_BIN"
    ;;
esac

echo "==> staged $PROFILE plugin -> $PLUGIN_DIR"
echo "      objectiveai.json"
echo "      cli/$(basename "$CLI_ZIP")"

# ── install the objectiveai host binaries (for integration tests) ──────────
# Skipped with --no-test (the release build only packages the plugin). Delegates
# to the upstream installer pinned to our objectiveai-sdk version tag; it caches
# the versioned release zip under .objectiveai/bin and reuses it on re-runs,
# then unpacks the host binaries into .objectiveai/bin.
if [ "$NO_TEST" = "0" ]; then
  # Handle both `objectiveai-sdk = "X"` and the table form
  # `objectiveai-sdk = { version = "X", features = [...] }` — grab the first
  # quoted token on the first objectiveai-sdk line (version precedes features).
  OAI_VERSION="$(grep -m1 -E '^objectiveai-sdk[[:space:]]*=' Cargo.toml \
    | sed -n -E 's/[^"]*"([^"]+)".*/\1/p')"
  [ -n "$OAI_VERSION" ] \
    || { echo "build.sh: could not read objectiveai-sdk version from Cargo.toml" >&2; exit 1; }
  echo "==> installing objectiveai host v$OAI_VERSION into .objectiveai/bin"
  curl -fsSL "https://raw.githubusercontent.com/ObjectiveAI/objectiveai/v$OAI_VERSION/install.sh" \
    | bash -s -- --no-export-path --objectiveai-dir "$REPO_ROOT/.objectiveai"

  # ── unpack the arcanum cli_zip in place ──────────────────────────────────
  # The staged zip alone isn't runnable; the sandbox needs the binary extracted
  # into cli/ (the zip stays for the release path).
  echo "==> unpacking arcanum plugin into the sandbox"
  case "$PLATFORM" in
    windows)
      powershell.exe -NoProfile -Command \
        "Expand-Archive -Force -LiteralPath '$(cygpath -w "$CLI_ZIP")' -DestinationPath '$(cygpath -w "$CLI_DIR")'"
      ;;
    *)
      unzip -o -q "$CLI_ZIP" -d "$CLI_DIR"
      ;;
  esac
fi
