#!/usr/bin/env bash
# Compile the grammars that ship NO prebuilt `.wasm` on npm into ./prebuilt/ — today
# markdown's two parsers (block + inline). Every other bundled language copies the
# `.wasm` out of its pinned npm package (gen-treesitter.mjs); markdown's package ships
# only C sources, so its wasm is built here once and committed. See prebuilt/README.md.
#
# Run only when the pinned @tree-sitter-grammars/tree-sitter-markdown version in
# `web/grammars.js` (VERSIONS) changes:
#   ./scripts/build-prebuilt-wasm.sh
# then commit the .wasm files and regenerate the vendored assets:
#   rm -rf vendor && npm run build:treesitter
#
# The grammar sources are fetched with `npm pack` at the version the registry pins, NOT
# from ./node_modules — nothing about the ordinary build should depend on a package whose
# only use is this rare rebuild, so it stays out of package.json / package-lock.json.
#
# Prereqs: emcc (emsdk or the system emscripten package) and the tree-sitter CLI
# (`cargo install tree-sitter-cli`, or `npx tree-sitter-cli@0.26.3`; override with
# $TREE_SITTER). The ordinary build never runs this — that's the point of committing
# the output.
set -euo pipefail
cd "$(dirname "$0")/.."

# emcc may live in an emsdk (sourced env) or the Arch system package — same discovery
# as ../build.sh, since `tree-sitter build --wasm` shells out to it.
if ! command -v emcc >/dev/null 2>&1; then
  if [ -f "$HOME/emsdk/emsdk_env.sh" ]; then
    # shellcheck disable=SC1091
    source "$HOME/emsdk/emsdk_env.sh" >/dev/null 2>&1
  elif [ -x /usr/lib/emscripten/emcc ]; then
    PATH="/usr/lib/emscripten:$PATH"
  fi
fi
command -v emcc >/dev/null 2>&1 || {
  echo "error: emcc not found — install emsdk or the system emscripten package first" >&2
  exit 1
}

TS_CLI=${TREE_SITTER:-tree-sitter}
command -v "$TS_CLI" >/dev/null 2>&1 || {
  echo "error: '$TS_CLI' not found — cargo install tree-sitter-cli (or set \$TREE_SITTER)" >&2
  exit 1
}

# The prebuilt set comes from the registry itself (`prebuilt` + `grammarDir` + the pinned
# version), so this script can never build a different file, from a different version,
# than the generator looks for. One `<out-wasm> <pkg> <version> <grammar-dir>` line each.
LANGS=$(node --input-type=module -e '
  import { REGISTRY, versionOf } from "../web/grammars.js";
  for (const [name, cfg] of Object.entries(REGISTRY)) {
    if (cfg.prebuilt) console.log(`${cfg.prebuilt} ${cfg.pkg} ${versionOf(cfg.pkg)} ${cfg.grammarDir}`);
  }
')
[ -n "$LANGS" ] || { echo "error: no registry entry declares a prebuilt wasm" >&2; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
OUT="$PWD/prebuilt"
mkdir -p "$OUT"

while read -r out pkg ver dir; do
  [ -n "$out" ] || continue
  src="$WORK/$(echo "$pkg" | tr '/@' '__')-$ver"
  if [ ! -d "$src" ]; then
    echo "fetching $pkg@$ver…"
    mkdir -p "$src"
    tgz=$(cd "$WORK" && npm pack "$pkg@$ver" --silent)
    tar xzf "$WORK/$tgz" -C "$src" --strip-components=1
  fi
  [ -f "$src/$dir/src/parser.c" ] || {
    echo "error: $pkg@$ver has no $dir/src/parser.c — did the package layout change?" >&2
    exit 1
  }
  echo "building prebuilt/$out from $pkg@$ver ($dir)…"
  ( cd "$src/$dir" && "$TS_CLI" build --wasm -o "$OUT/$out" )
  chmod 644 "$OUT/$out"
done <<< "$LANGS"

# Sanity: each output must load under the pinned web-tree-sitter and parse. The generator
# additionally proves the queries produce captures against it (0 captures ⇒ build error).
node --input-type=module -e '
  import { Parser, Language } from "web-tree-sitter";
  import { readdirSync } from "node:fs";
  await Parser.init();
  for (const f of readdirSync("prebuilt").filter((f) => f.endsWith(".wasm"))) {
    const lang = await Language.load(`prebuilt/${f}`);
    const p = new Parser();
    p.setLanguage(lang);
    if (p.parse("# hi\n\nsome *text*\n").rootNode.childCount === 0) {
      throw new Error(`${f}: loaded but parsed nothing`);
    }
    console.log(`  ${f.padEnd(24)} ABI ${lang.abiVersion} — loads + parses`);
  }
'
echo "done — commit prebuilt/*.wasm, then: rm -rf vendor && npm run build:treesitter"
