// bemtvi-regex shim for nvim/os/os_defs.h: just the libc includes and the one
// constant the vendored sources use. No libuv, no auto/config.h.
#pragma once

#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define MAXPATHL 4096
