// nxvim-regex shim: expression evaluation is not available in the vendored
// engine (\= substitution fails loud in regexp.c).
#pragma once
#include "nvim/eval/typval.h"
