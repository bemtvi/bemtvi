#!/usr/bin/env bash
# Write THIRD-PARTY-LICENSES.txt: the licenses of everything bemtvi's downloads
# contain that bemtvi didn't write. build.yml ships it in every archive, next
# to bemtvi's own LICENSE.
#
#   scripts/gen-licenses.sh native   OUT   # the terminal editor and the GUI
#   scripts/gen-licenses.sh edithost OUT   # the browser build's static site
#
# The Rust crates come from cargo-about (licenses/about.toml); the code that
# isn't a crate is listed here by hand. Add to it when vendoring something.
# `edithost` runs after crates/bemtvi-edithost/build.sh, whose npm installs
# and emscripten it reads the licenses from.
#
# Needs cargo-about (`cargo install --locked cargo-about`) and jq.
set -euo pipefail

mode="${1:-}"
out="${2:-}"
if [ -z "$out" ] || { [ "$mode" != native ] && [ "$mode" != edithost ]; }; then
  echo "usage: $0 native|edithost OUT" >&2
  exit 2
fi
root="$(cd "$(dirname "$0")/.." && pwd)"
eh="$root/crates/bemtvi-edithost"
out="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"
cd "$root"

need() {
  [ -f "$1" ] || { echo "error: $1 is missing" >&2; exit 1; }
}

# One vendored component: a heading, what it is, where it's used, and its
# license text (a file, or stdin with `-`).
component() { # <name> <what and where> <license file | ->
  printf '\n--------------------------------------------------------------------------------\n'
  printf '%s\n\n%s\n\n' "$1" "$2"
  if [ "$3" = - ]; then cat; else need "$3"; cat "$3"; fi
  printf '\n'
}

if [ "$mode" = native ]; then
  manifest="$root/Cargo.toml"
else
  manifest="$eh/Cargo.toml"
fi

# The vendored Lua's copyright notice is the comment at the end of its lua.h.
lua_src=$(cargo metadata --format-version 1 --manifest-path "$manifest" \
  | jq -r '.packages[] | select(.name == "lua-src") | .manifest_path' | head -1)
[ -n "$lua_src" ] || { echo "error: lua-src isn't in the dependency tree" >&2; exit 1; }
# cargo gives Windows paths on Windows (Git Bash), which dirname can't split.
if command -v cygpath >/dev/null; then lua_src=$(cygpath -u "$lua_src"); fi
lua_h=$(ls "$(dirname "$lua_src")"/lua-5.4*/lua.h | tail -1)
need "$lua_h"

# QuickJS, which rquickjs-sys vendors (native only: :TSInstall's grammar generator).
if [ "$mode" = native ]; then
  rquickjs_sys=$(cargo metadata --format-version 1 --manifest-path "$manifest" \
    | jq -r '.packages[] | select(.name == "rquickjs-sys") | .manifest_path' | head -1)
  [ -n "$rquickjs_sys" ] || { echo "error: rquickjs-sys isn't in the dependency tree" >&2; exit 1; }
  if command -v cygpath >/dev/null; then rquickjs_sys=$(cygpath -u "$rquickjs_sys"); fi
  quickjs_license="$(dirname "$rquickjs_sys")/quickjs/LICENSE"
  need "$quickjs_license"
fi

regex="$root/crates/bemtvi-regex"
colors="$root/crates/bemtvi-server/runtime/colors"

{
  cat <<'EOF'
Third-party licenses
====================

bemtvi is licensed under the Apache License 2.0 (the LICENSE file beside this
one). It contains the third-party code below, each under its own license.

Vendored code
=============
EOF

  sed -n '/^\* Copyright (C) 1994/,/^\*\*\*\*/p' "$lua_h" | sed -e '$d' -e 's/^\* \{0,1\}//' \
    | component "Lua 5.4" "The Lua interpreter that runs configs and plugins (vendored by the lua-src crate)." -

  component "Neovim" \
    "The vim regular expression engines and their supporting code (crates/bemtvi-regex), and the bundled colorschemes (crates/bemtvi-server/runtime/colors), are from Neovim, which includes code under the Vim license. bemtvi modified the regular expression code; the modified source is in crates/bemtvi-regex/csrc of https://github.com/bemtvi/bemtvi, at the tag of this release, and its maintainer can be reached through https://github.com/bemtvi/bemtvi/issues." \
    "$regex/LICENSE.txt"
  cmp -s "$regex/LICENSE.txt" "$colors/LICENSE.txt" || {
    echo "error: $colors/LICENSE.txt differs from $regex/LICENSE.txt; list it on its own" >&2
    exit 1
  }

  component "utf8proc" "Unicode properties for the vim regular expression engines (crates/bemtvi-regex/vendor/utf8proc)." \
    "$regex/vendor/utf8proc/LICENSE.md"

  need "$regex/csrc/klib/kvec.h"
  sed -n '1,/^$/p' "$regex/csrc/klib/kvec.h" | sed 's|^// \{0,1\}||' \
    | component "klib (kvec.h)" "A vector macro library used by the vim regular expression engines." -

  if [ "$mode" = native ]; then
    component "QuickJS" "The JavaScript engine that evaluates a grammar's grammar.js when :TSInstall generates a parser (vendored by the rquickjs-sys crate)." \
      "$quickjs_license"
    component "tree-sitter grammar DSL" "The grammar DSL that :TSInstall evaluates grammar.js with (crates/bemtvi-ts/src/generate/dsl.js), from the tree-sitter-generate crate." \
      "$root/crates/bemtvi-ts/src/generate/LICENSE"
  fi

  if [ "$mode" = edithost ]; then
    emcc_path=$(command -v emcc) || { echo "error: emcc isn't on PATH" >&2; exit 1; }
    em="$(dirname "$(readlink -f "$emcc_path")")"
    component "Emscripten" "The compiler runtime linked into dist/eh.wasm." "$em/LICENSE"
    component "musl libc" "The C library linked into dist/eh.wasm (through Emscripten)." "$em/system/lib/libc/musl/COPYRIGHT"
    component "LLVM libc++, libc++abi and compiler-rt" "The C++ and compiler runtime libraries linked into dist/eh.wasm (through Emscripten)." \
      "$em/system/lib/libcxx/LICENSE.TXT"

    ts="$eh/treesitter/node_modules"
    component "web-tree-sitter" "The tree-sitter runtime for syntax highlighting (web/vendor/web-tree-sitter)." "$ts/web-tree-sitter/LICENSE"

    # Each bundled grammar (web/vendor/grammars), from the build's manifest. A
    # language this list doesn't know fails, so a newly bundled grammar can't
    # ship without its license.
    vendor_manifest="$eh/web/vendor/manifest.json"
    need "$vendor_manifest"
    for lang in $(jq -r '.languages[]' "$vendor_manifest"); do
      case "$lang" in
        rust)       lic="$ts/tree-sitter-rust/LICENSE" ;;
        json)       lic="$ts/tree-sitter-json/LICENSE" ;;
        javascript) lic="$ts/tree-sitter-javascript/LICENSE" ;;
        typescript) lic="$ts/tree-sitter-typescript/LICENSE" ;;
        python)     lic="$ts/tree-sitter-python/LICENSE" ;;
        lua)        lic="$ts/@tree-sitter-grammars/tree-sitter-lua/LICENSE.md" ;;
        markdown | markdown_inline) continue ;;
        *) echo "error: no license known for the bundled $lang grammar; add it to $0" >&2; exit 1 ;;
      esac
      component "tree-sitter grammar: $lang" "The $lang parser and its highlight queries (web/vendor/grammars/$lang.wasm)." "$lic"
    done
    if jq -e '.languages | index("markdown")' "$vendor_manifest" >/dev/null; then
      component "tree-sitter grammar: markdown" "The markdown and markdown_inline parsers (web/vendor/grammars), built from tree-sitter-grammars/tree-sitter-markdown." \
        "$eh/treesitter/prebuilt/LICENSE-tree-sitter-markdown"
    fi

    component "nvim-treesitter and nvim-treesitter-textobjects" \
      "The indent, fold, text-object and markdown highlight queries (web/vendor/{indents,folds,textobjects,queries}) are from https://github.com/nvim-treesitter/nvim-treesitter and https://github.com/nvim-treesitter/nvim-treesitter-textobjects, under the Apache License 2.0, whose text is in the LICENSE file beside this one." \
      - </dev/null

    component "@msgpack/msgpack" "The msgpack codec of the daemon connection (web/vendor/msgpack)." \
      "$eh/web/node_modules/@msgpack/msgpack/LICENSE"
  fi

  printf '\n'
  cargo about generate --fail --manifest-path "$manifest" -c licenses/about.toml licenses/about.hbs
} > "$out"

echo "wrote $out"
