// nxvim-regex shim for nvim/plines.h: virtual-column support for the \%v
// family of assertions. Implemented in shim/nxre_shim.c with tabstop +
// utf8proc character widths (a close, documented approximation of vim's
// charsize machinery).
#pragma once

#include "nvim/buffer_defs.h"
#include "nvim/pos_defs.h"

int win_linetabsize(win_T *wp, linenr_T lnum, char *line, colnr_T len);
void getvvcol(win_T *wp, pos_T *pos, colnr_T *start, colnr_T *cursor, colnr_T *end, int flags);
