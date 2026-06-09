#!/usr/bin/env bash
# Netlify build entry for the nxvim web UI.
#
# Netlify's Git integration runs this on every push to the production branch (see
# ../../netlify.toml). The ordinary build.sh assumes a Rust→wasm toolchain is
# already present; on Netlify's build image we must provision it first, then hand
# off. The published directory (crates/nxvim-web/web) is set in netlify.toml.
#
# Node is provided by the image (NODE_VERSION in netlify.toml); the Rust version is
# provisioned by Netlify from RUST_VERSION there. Here we add the wasm target and a
# version-pinned wasm-bindgen, then run the normal build.
set -euo pipefail

# Must match `wasm-bindgen` in Cargo.toml — the CLI and the crate have to agree.
WASM_BINDGEN_VERSION=0.2.123

here="$(cd "$(dirname "$0")" && pwd)"

# 1. Rust + the wasm target. Netlify provisions Rust (RUST_VERSION) with rustup on
#    PATH; install rustup ourselves only if a future image drops it.
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
rustup target add wasm32-unknown-unknown

# 2. wasm-bindgen CLI, pinned. Download the prebuilt binary (seconds) instead of
#    `cargo install` (minutes), unless the right version is already on PATH.
if ! command -v wasm-bindgen >/dev/null 2>&1 \
   || [ "$(wasm-bindgen --version | awk '{print $2}')" != "$WASM_BINDGEN_VERSION" ]; then
  pkg="wasm-bindgen-${WASM_BINDGEN_VERSION}-x86_64-unknown-linux-musl"
  bindir="$HOME/.local/wasm-bindgen-${WASM_BINDGEN_VERSION}"
  mkdir -p "$bindir"
  curl -fsSL "https://github.com/rustwasm/wasm-bindgen/releases/download/${WASM_BINDGEN_VERSION}/${pkg}.tar.gz" \
    | tar -xz -C "$bindir" --strip-components=1 "${pkg}/wasm-bindgen"
  export PATH="$bindir:$PATH"
fi

# 3. Build the static bundle: npm (Tailwind + tree-sitter vendoring) → cargo wasm →
#    wasm-bindgen, emitting into crates/nxvim-web/web/{pkg,vendor,tailwind.css}.
exec bash "$here/build.sh"
