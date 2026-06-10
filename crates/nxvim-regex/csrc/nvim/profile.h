// nxvim-regex shim for nvim/profile.h: the engine only checks time limits.
// proftime_T comes from types_defs.h (uint64_t).
#pragma once

#include <stdbool.h>

#include "nvim/types_defs.h"

bool profile_passed_limit(proftime_T tm);
