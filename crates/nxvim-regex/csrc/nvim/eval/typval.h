// nxvim-regex shim for nvim/eval/typval.h. Expression substitution (\=) is
// not compiled into the vendored engine (see the NXVIM patches in regexp.c),
// so typval_T/list_T only appear as never-dereferenced pointers.
#pragma once

#include "nvim/eval/typval_defs.h"
