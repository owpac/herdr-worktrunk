#!/bin/sh
# Put the herdr-worktrunk binary at bin/herdr-worktrunk, for this checkout.
#
# herdr runs this script as the [[build]] step of `herdr plugin install` (an update is a new
# install). `herdr plugin link` does not run it: run it yourself for a local checkout.
# After an install by herdr, the script gives the new binary to the running observer.
#
# The script downloads the binary of the manifest version from the GitHub release, and checks it
# with the SHA256SUMS file of the release. The tag of a release is its version (YYYY.M.N).
# The download needs the commit of the release binary: the release commit of this version, whose
# subject starts with "chore: release <version>", or the commit of the tag of this version. main
# keeps the version of its last release: a git checkout of a later commit has no release binary,
# so the script builds with cargo. The version 0.0.0 has no release too.
# If the release has no binary for this machine, or if the download or the check fails, the script
# also builds the binary with cargo (Rust 1.89 or later).
#
#   HERDR_WORKTRUNK_BUILD=source   skip the download, and build with cargo
set -u

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd) || exit 1
cd "$ROOT" || exit 1
REPOSITORY_URL="https://github.com/owpac/herdr-worktrunk"
TARGET="$ROOT/bin/herdr-worktrunk"

say() { printf 'herdr-worktrunk: %s\n' "$*" >&2; }

version=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' herdr-plugin.toml | head -n 1)
if [ -z "$version" ]; then
  say "cannot read the version in herdr-plugin.toml"
  exit 1
fi

# The name of the release binary for this machine: herdr-worktrunk-<os>-<arch>.
asset_name() {
  case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) return 1 ;;
  esac
  arch=$(uname -m)
  # A shell under Rosetta shows x86_64 on Apple silicon: use the native binary.
  if [ "$os" = macos ] && [ "$(sysctl -n hw.optional.arm64 2>/dev/null)" = 1 ]; then
    arch=arm64
  fi
  case "$arch" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) return 1 ;;
  esac
  printf 'herdr-worktrunk-%s-%s\n' "$os" "$arch"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

# Put the file $1 at bin/herdr-worktrunk. Fail if a step fails.
install_binary() {
  mkdir -p bin || return 1
  mv "$1" "$TARGET" || return 1
  chmod +x "$TARGET" || return 1
}

download() {
  asset=$(asset_name) || { say "no release binary for this machine"; return 1; }
  command -v curl >/dev/null 2>&1 || { say "curl is missing"; return 1; }
  if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
    say "sha256sum and shasum are missing: the script cannot check the release binary"
    return 1
  fi
  base="$REPOSITORY_URL/releases/download/$version"
  work=$(mktemp -d) || return 1
  if ! curl -fsSL -o "$work/$asset" "$base/$asset" || ! curl -fsSL -o "$work/SHA256SUMS" "$base/SHA256SUMS"; then
    say "cannot download $asset of $version"
    rm -rf "$work"
    return 1
  fi
  expected=$(awk -v name="$asset" '$2 == name { print $1 }' "$work/SHA256SUMS")
  if [ -z "$expected" ] || [ "$expected" != "$(sha256_of "$work/$asset")" ]; then
    say "WARNING: the checksum of $asset does not match SHA256SUMS: the script does not use this binary"
    rm -rf "$work"
    return 1
  fi
  if ! install_binary "$work/$asset"; then
    say "cannot install $asset at $TARGET"
    rm -rf "$work"
    return 1
  fi
  rm -rf "$work"
  say "installed the release binary $asset of $version"
}

build_from_source() {
  command -v cargo >/dev/null 2>&1 || { say "cargo is missing: install Rust 1.89 or later"; return 1; }
  cargo build --release --locked || return 1
  cp target/release/herdr-worktrunk "$ROOT/bin.tmp-herdr-worktrunk" || return 1
  install_binary "$ROOT/bin.tmp-herdr-worktrunk" || { say "cannot install the binary at $TARGET"; return 1; }
  say "built herdr-worktrunk $version with cargo"
}

# True if the checkout is the commit of the release binary: the release commit of the version, or
# the commit of its tag. A checkout without git, for example an archive, counts as the release
# commit.
release_commit() {
  [ -e .git ] || return 0
  case "$(git log -1 --format=%s 2>/dev/null)" in
    "chore: release $version" | "chore: release $version "*) return 0 ;;
  esac
  # The tag can point to a later commit of main: a merge during the run of the release workflow.
  # herdr makes a shallow clone without the tags: ask the remote. An annotated tag gives 2 lines,
  # and the last line is its commit.
  head=$(git rev-parse HEAD 2>/dev/null) || head=""
  tag=$(git ls-remote origin "refs/tags/$version" "refs/tags/$version^{}" 2>/dev/null | awk 'END { print $1 }')
  if [ -n "$head" ] && [ "$head" = "$tag" ]; then
    return 0
  fi
  say "this commit is not the release commit of $version: the script builds with cargo"
  return 1
}

# Give the new binary to the running observer: the observer runs it in about 2 s. herdr installs each
# version in a new folder and runs no command of the plugin after this build: without this step, the
# observer keeps the binary of the old version until the next start of herdr, the next new agent or
# the next action of the plugin. Only an install by herdr does it: herdr gives HERDR_PLUGIN_ROOT, or
# the checkout is in the install folder of herdr. A build in a development checkout keeps the
# observer of the installed plugin.
hand_over() {
  case "${HERDR_PLUGIN_ROOT:+herdr}:$ROOT" in
    herdr:* | */plugins/github-installations/*)
      "$TARGET" observe --hand-over || say "cannot give the new binary to the observer"
      ;;
  esac
}

if [ "${HERDR_WORKTRUNK_BUILD:-}" != source ] && [ "$version" != 0.0.0 ] && release_commit && download; then
  hand_over
  exit 0
fi
build_from_source || exit 1
hand_over
