// bemtvi-regex shim for nvim/mark.h. mark_get() is implemented in
// shim/btvre_shim.c on top of the host-registered mark provider; without a
// provider every mark reads as "not set" (matching vim's NOMATCH semantics
// for unset marks).
#pragma once

#include "nvim/buffer_defs.h"
#include "nvim/mark_defs.h"

fmark_T *mark_get(buf_T *buf, win_T *win, fmark_T *fmp, MarkGet flag, int name);
