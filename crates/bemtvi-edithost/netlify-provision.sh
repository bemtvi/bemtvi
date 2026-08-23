#!/usr/bin/env bash
# Shared Netlify toolchain provisioning for the wasm edit-host, **sourced** by both deploy
# entries (netlify-build.sh = the standard editor; netlify-build-demo.sh = the python demo).
# It installs/selects the Rust→wasm toolchain and the Emscripten `emcc` linker, leaving both
# on PATH for the caller's `build.sh`. Node is provided by the image (NODE_VERSION).
#
# Source it (so the PATH/env edits persist into the caller):  . "$here/netlify-provision.sh"
# Pinned, overridable from netlify.toml's [build.environment].
TOOLCHAIN="${RUST_VERSION:-stable}"
EMSDK_VERSION="${EMSDK_VERSION:-6.0.0}"

# 1. Rust toolchain + both wasm targets. Netlify's image ships `rustup` but with no
#    default toolchain configured, so install & select it explicitly. No wasm-bindgen — this
#    build links via emcc. The bare `wasm32-unknown-unknown` target is for the column-math
#    module (bemtvi-width): no emscripten and no JS glue, instantiated by the page itself.
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain none --profile minimal
  # shellcheck disable=SC1091
  . "$HOME/.cargo/env"
fi
rustup toolchain install "$TOOLCHAIN" --profile minimal
rustup default "$TOOLCHAIN"
rustup target add wasm32-unknown-emscripten wasm32-unknown-unknown

# 2. Emscripten SDK (emcc). Install into $HOME/emsdk — the exact path build.sh probes and
#    sources when emcc isn't already on PATH. Downloaded prebuilt (a couple of minutes), not
#    compiled; emsdk's dir is cacheable across builds.
if ! command -v emcc >/dev/null 2>&1; then
  if [ ! -d "$HOME/emsdk" ]; then
    git clone --depth 1 https://github.com/emscripten-core/emsdk.git "$HOME/emsdk"
  fi
  ( cd "$HOME/emsdk" && ./emsdk install "$EMSDK_VERSION" && ./emsdk activate "$EMSDK_VERSION" )
  # shellcheck disable=SC1091
  . "$HOME/emsdk/emsdk_env.sh"
fi
