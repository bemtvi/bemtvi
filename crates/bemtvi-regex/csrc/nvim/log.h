// bemtvi-regex shim for nvim/log.h: logging macros are no-ops here (garray
// uses WLOG for growth warnings only).
#pragma once

#define WLOG(...) ((void)0)
#define ELOG(...) ((void)0)
#define DLOG(...) ((void)0)
