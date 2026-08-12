#!/usr/bin/env bash
# Regenerate the *.generated.h prototype headers for the vendored neovim
# sources in csrc/nvim, using neovim's own declaration generator (which needs
# `nvim` on PATH for vim.lpeg). Run from the crate root after re-vendoring
# any file from vendor/neovim.
set -euo pipefail

cd "$(dirname "$0")"
CRATE=$PWD
NVIM_SRC=$(cd ../../vendor/neovim/src && pwd)
OUT=$CRATE/csrc/nvim

if [ ! -f "$NVIM_SRC/gen/gen_declarations.lua" ]; then
  echo "error: vendor/neovim submodule not populated (need src/gen/gen_declarations.lua)" >&2
  exit 1
fi

gen() { # gen <file.c|file.h> -> .c.generated.h/.h.generated.h or .h.inline.generated.h
  local f=$1 base
  base=$(basename "$f")
  # run from $NVIM_SRC so the generator can require('gen.c_grammar')
  case "$f" in
  *.c)
    (cd "$NVIM_SRC" && nvim -l gen/gen_declarations.lua "$OUT/$f" \
      "$OUT/${f%.c}.c.generated.h" "$OUT/${f%.c}.h.generated.h" "$base.generated.h")
    ;;
  *.h)
    (cd "$NVIM_SRC" && nvim -l gen/gen_declarations.lua "$OUT/$f" \
      "$OUT/${f%.h}.h.inline.generated.h" SKIP "${base%.h}.h.inline.generated.h")
    ;;
  esac
}

# .c files vendored (possibly as subsets) in csrc/nvim
gen regexp.c
gen garray.c
gen mbyte.c
gen charset.c
# strings.c: publics are hand-declared in the strings.h shim
# .h files that contain `static inline` definitions
gen mbyte.h
gen charset.h
# strings.h is a hand-written shim (no inline functions)

echo "generated headers written to $OUT"

# ascii_defs.h has static-inline helpers too
gen ascii_defs.h
