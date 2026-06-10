// nxvim-regex shim for nvim/eval/typval_defs.h: opaque typval types; the
// vendored engine never dereferences them (\= substitution is patched out).
#pragma once

typedef struct typval_S typval_T;
typedef struct list_S list_T;
