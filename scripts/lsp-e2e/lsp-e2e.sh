#!/usr/bin/env bash
#
# lsp-e2e.sh — install the pinned language servers the LSP end-to-end suite drives.
#
# The suite (crates/nxvim/tests/lsp_e2e.rs) is gated behind NXVIM_LSP_E2E=1 and runs
# the *real* server binaries, configured through the vendored nvim-lspconfig. This
# script downloads exactly the versions pinned in manifest.json, verifies their
# hashes, and installs them into a local, git-ignored prefix.
#
# Every download is hash-checked and FAILS LOUDLY on mismatch — there are no silent
# fallbacks. Four mechanisms, one per ecosystem:
#   * GitHub release binaries (rust-analyzer / lua_ls / clangd): pinned sha256.
#   * npm servers (pyright / ts / bash / json+eslint / yaml): `npm ci` verifies every
#     package's integrity hash against the committed package-lock.json.
#   * gopls: `go install` at a pinned version; the module checksum is verified against
#     manifest.json's `sum` (and again by the Go checksum database).
#
# Usage:
#   scripts/lsp-e2e/lsp-e2e.sh install     # download + verify + install all servers
#   scripts/lsp-e2e/lsp-e2e.sh doctor      # report which servers are present/runnable
#   scripts/lsp-e2e/lsp-e2e.sh lock        # re-derive binary sha256 + npm lockfile (maintainers)
#   scripts/lsp-e2e/lsp-e2e.sh print-bin   # print the install bin dir (for $PATH / the test)
#
# The install prefix defaults to <repo>/.lsp-e2e and can be overridden with
# NXVIM_LSP_E2E_DIR. The test reads the same default, so `install` then
# `NXVIM_LSP_E2E=1 cargo test -p nxvim --test lsp_e2e` just works.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MANIFEST="$SCRIPT_DIR/manifest.json"
NPM_DIR="$SCRIPT_DIR/npm"

PREFIX="${NXVIM_LSP_E2E_DIR:-$REPO_ROOT/.lsp-e2e}"
BIN_DIR="$PREFIX/bin"
PKG_DIR="$PREFIX/pkg"

# ----- platform detection ---------------------------------------------------

detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os/$arch" in
    Darwin/arm64) echo "darwin-arm64" ;;
    Linux/x86_64) echo "linux-x86_64" ;;
    *)
      echo "lsp-e2e: unsupported host '$os/$arch' — the suite pins hashes only for" \
           "darwin-arm64 and linux-x86_64 (see manifest.json)." >&2
      exit 1
      ;;
  esac
}

PLATFORM="$(detect_platform)"

# ----- small helpers --------------------------------------------------------

log()  { printf '\033[1;34m[lsp-e2e]\033[0m %s\n' "$*"; }
ok()   { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m[lsp-e2e] error:\033[0m %s\n' "$*" >&2; exit 1; }

need() { command -v "$1" >/dev/null 2>&1 || die "required tool '$1' not found on PATH"; }

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  else shasum -a 256 "$1" | awk '{print $1}'; fi
}

# curl with retries on transient errors (GitHub release CDN 5xx/504 are common).
fetch() {
  local out="$1" url="$2"
  curl -fsSL -m 600 --retry 5 --retry-delay 3 --retry-all-errors -o "$out" "$url"
}

# Read a value out of manifest.json with jq.
m() { jq -r "$1" "$MANIFEST"; }

# Verify $1 (a file) hashes to $2, or die.
# Write a launcher at $1 that execs the real binary $2 by its absolute path. Tree
# servers (lua_ls, clangd) locate their own runtime files (main.lua, lib/…)
# relative to the *invoked* path, so a bare symlink — whose dir holds none of those
# files — breaks them. A wrapper that execs the real path keeps argv[0] correct.
write_launcher() {
  local launcher="$1" real="$2"
  # Remove any existing target FIRST. A prior install may have left a symlink
  # here; `cat >` would follow it and overwrite the *real* binary it points at
  # (turning the binary into a wrapper that execs itself). `rm -f` breaks the link
  # so we write a fresh regular file.
  rm -f "$launcher"
  cat > "$launcher" <<EOF
#!/bin/sh
exec "$real" "\$@"
EOF
  chmod +x "$launcher"
}

verify_sha256() {
  local file="$1" want="$2" got
  got="$(sha256 "$file")"
  [ "$got" = "$want" ] || die "sha256 mismatch for $(basename "$file")
    expected: $want
    actual:   $got
  Refusing to install an unverified binary. If you intentionally bumped a version,
  re-run 'lsp-e2e.sh lock' and review the manifest diff."
  ok "sha256 verified ($want)"
}

# ----- GitHub release binaries ---------------------------------------------

# Substitute {version} and the per-binary platform token into a URL template.
binary_url() {
  local name="$1" version url tokenkey tokenval
  version="$(m ".binaries.\"$name\".version")"
  url="$(m ".binaries.\"$name\".url")"
  # Each binary names its own platform-token key (rust_triple / lua_plat / clangd_plat).
  tokenkey="$(jq -r ".binaries.\"$name\" | to_entries[] | select(.value | type==\"object\") | .key" "$MANIFEST" | grep -vx "sha256" | head -1)"
  tokenval="$(m ".binaries.\"$name\".\"$tokenkey\".\"$PLATFORM\"")"
  url="${url//\{version\}/$version}"
  url="${url//\{$tokenkey\}/$tokenval}"
  echo "$url"
}

install_binary() {
  local name="$1"
  local version url want fmt tmp
  version="$(m ".binaries.\"$name\".version")"
  url="$(binary_url "$name")"
  want="$(m ".binaries.\"$name\".sha256.\"$PLATFORM\"")"
  fmt="$(m ".binaries.\"$name\".format")"
  log "$name $version  ($PLATFORM)"
  echo "      $url"

  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  local archive="$tmp/asset"
  fetch "$archive" "$url" || die "download failed: $url"
  verify_sha256 "$archive" "$want"

  local target_bin="$BIN_DIR/$(m ".binaries.\"$name\".bin")"
  case "$fmt" in
    gz)
      # `rm -f` first so we never write through a leftover symlink (see write_launcher).
      rm -f "$target_bin"
      gunzip -c "$archive" > "$target_bin"
      chmod +x "$target_bin"
      ;;
    tar.gz | tar.xz)
      local dest="$PKG_DIR/$name"
      rm -rf "$dest"; mkdir -p "$dest"
      # `tar -xf` auto-detects gzip/xz (both BSD/libarchive and GNU tar handle it).
      tar -xf "$archive" -C "$dest"
      local rel; rel="$(m ".binaries.\"$name\".bin_in_archive")"
      rel="${rel//\{version\}/$version}"
      [ -f "$dest/$rel" ] || die "expected '$rel' inside $name archive, not found"
      chmod +x "$dest/$rel"
      write_launcher "$target_bin" "$dest/$rel"
      ;;
    zip)
      local dest="$PKG_DIR/$name"
      rm -rf "$dest"; mkdir -p "$dest"
      unzip -q -o "$archive" -d "$dest"
      local rel; rel="$(m ".binaries.\"$name\".bin_in_archive")"
      rel="${rel//\{version\}/$version}"
      [ -f "$dest/$rel" ] || die "expected '$rel' inside $name archive, not found"
      chmod +x "$dest/$rel"
      write_launcher "$target_bin" "$dest/$rel"
      ;;
    *) die "unknown format '$fmt' for $name" ;;
  esac
  ok "installed -> $target_bin"
  trap - RETURN
  rm -rf "$tmp"
}

# ----- gopls (go install, sum-verified) ------------------------------------

install_gopls() {
  need go
  local module version want got
  module="$(m '.go.gopls.module')"
  version="$(m '.go.gopls.version')"
  want="$(m '.go.gopls.sum')"
  log "gopls $version  (go install)"

  # Verify the pinned module checksum before building. `go mod download` also
  # checks it against the Go checksum database; we additionally pin it ourselves.
  got="$(GOFLAGS= go mod download -json "$module@$version" 2>/dev/null | jq -r '.Sum')"
  [ "$got" = "$want" ] || die "gopls module sum mismatch
    expected: $want
    actual:   $got"
  ok "module sum verified ($want)"

  GOBIN="$BIN_DIR" go install "$module@$version" || die "go install gopls failed"
  ok "installed -> $BIN_DIR/gopls"
}

# ----- npm servers (npm ci, integrity-verified) ----------------------------

install_npm() {
  need npm
  [ -f "$NPM_DIR/package-lock.json" ] || die "missing $NPM_DIR/package-lock.json — run 'lsp-e2e.sh lock' first"
  log "node servers  (npm ci — integrity-verified from package-lock.json)"
  ( cd "$NPM_DIR" && npm ci --no-audit --no-fund ) || die "npm ci failed"

  # Link each server's launcher from node_modules/.bin into our shared bin dir.
  local key bin
  for key in $(jq -r '.npm.servers | keys[]' "$MANIFEST"); do
    bin="$(m ".npm.servers.\"$key\".bin")"
    local src="$NPM_DIR/node_modules/.bin/$bin"
    [ -e "$src" ] || die "npm server '$bin' missing after install (expected $src)"
    ln -sf "$src" "$BIN_DIR/$bin"
    ok "linked -> $BIN_DIR/$bin"
  done
}

# ----- subcommands ----------------------------------------------------------

cmd_install() {
  need curl; need jq; need tar; need unzip; need gunzip
  [ -f "$MANIFEST" ] || die "manifest not found: $MANIFEST"
  mkdir -p "$BIN_DIR" "$PKG_DIR"
  log "installing into $PREFIX  (platform: $PLATFORM)"

  # Optional positional args select a subset (server name, "gopls", or "npm");
  # with no args, install everything.
  local want_all=1; declare -A want=()
  if [ "$#" -gt 0 ]; then want_all=0; local a; for a in "$@"; do want["$a"]=1; done; fi
  sel() { [ "$want_all" = 1 ] || [ -n "${want[$1]:-}" ]; }

  local name
  for name in $(jq -r '.binaries | keys[]' "$MANIFEST"); do
    sel "$name" && install_binary "$name"
  done
  sel gopls && install_gopls
  sel npm && install_npm

  echo
  log "done. Add to PATH for the suite:  export PATH=\"$BIN_DIR:\$PATH\""
  log "then:  NXVIM_LSP_E2E=1 cargo test -p nxvim --test lsp_e2e -- --nocapture"
}

cmd_doctor() {
  [ -f "$MANIFEST" ] || die "manifest not found: $MANIFEST"
  log "checking servers in $BIN_DIR"
  local all_ok=1 name bin
  # single binaries
  for name in $(jq -r '.binaries | keys[]' "$MANIFEST"); do
    bin="$(m ".binaries.\"$name\".bin")"; report_bin "$bin" || all_ok=0
  done
  report_bin gopls || all_ok=0
  for name in $(jq -r '.npm.servers | keys[]' "$MANIFEST"); do
    bin="$(m ".npm.servers.\"$name\".bin")"; report_bin "$bin" || all_ok=0
  done
  [ "$all_ok" = 1 ] && ok "all servers present" || die "some servers are missing — run 'lsp-e2e.sh install'"
}

report_bin() {
  local bin="$1" path="$BIN_DIR/$1"
  if [ -x "$path" ] || [ -L "$path" ]; then
    printf '\033[1;32m  ✓\033[0m %-32s %s\n' "$bin" "$path"; return 0
  else
    printf '\033[1;31m  ✗\033[0m %-32s missing\n' "$bin"; return 1
  fi
}

# Maintainer-only: re-derive single-binary sha256 for BOTH platforms and rewrite
# manifest.json, then refresh the npm lockfile. Run after bumping a version.
cmd_lock() {
  need curl; need jq
  log "re-deriving binary hashes for darwin-arm64 + linux-x86_64"
  local tmp; tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
  local name plat updated="$tmp/manifest.json"
  cp "$MANIFEST" "$updated"
  for name in $(jq -r '.binaries | keys[]' "$MANIFEST"); do
    for plat in darwin-arm64 linux-x86_64; do
      local url; url="$(PLATFORM="$plat" binary_url_for "$name" "$plat")"
      log "  $name [$plat]: $url"
      fetch "$tmp/a" "$url" || die "download failed: $url"
      local h; h="$(sha256 "$tmp/a")"
      jq ".binaries.\"$name\".sha256.\"$plat\" = \"$h\"" "$updated" > "$tmp/n" && mv "$tmp/n" "$updated"
      ok "$name [$plat] = $h"
    done
  done
  mv "$updated" "$MANIFEST"
  log "refreshing gopls module sum"
  local sum; sum="$(go mod download -json "$(m '.go.gopls.module')@$(m '.go.gopls.version')" | jq -r '.Sum')"
  jq ".go.gopls.sum = \"$sum\"" "$MANIFEST" > "$tmp/n" && mv "$tmp/n" "$MANIFEST"
  ok "gopls sum = $sum"
  log "refreshing npm lockfile (npm install)"
  ( cd "$NPM_DIR" && npm install --no-audit --no-fund --package-lock-only )
  ok "lockfiles refreshed — review the diff before committing"
}

# Like binary_url but with an explicit platform arg (used by lock for both platforms).
binary_url_for() {
  local name="$1" plat="$2" version url tokenkey tokenval
  version="$(m ".binaries.\"$name\".version")"
  url="$(m ".binaries.\"$name\".url")"
  tokenkey="$(jq -r ".binaries.\"$name\" | to_entries[] | select(.value | type==\"object\") | .key" "$MANIFEST" | grep -vx "sha256" | head -1)"
  tokenval="$(m ".binaries.\"$name\".\"$tokenkey\".\"$plat\"")"
  url="${url//\{version\}/$version}"
  url="${url//\{$tokenkey\}/$tokenval}"
  echo "$url"
}

cmd_print_bin() { echo "$BIN_DIR"; }

case "${1:-}" in
  install)   shift; cmd_install "$@" ;;
  doctor)    cmd_doctor ;;
  lock)      cmd_lock ;;
  print-bin) cmd_print_bin ;;
  *) cat >&2 <<EOF
usage: lsp-e2e.sh <command>
  install     download + verify + install all pinned servers into $PREFIX
  doctor      report which servers are present
  lock        (maintainers) re-derive hashes + npm lockfile after a version bump
  print-bin   print the install bin dir
EOF
     exit 2 ;;
esac
