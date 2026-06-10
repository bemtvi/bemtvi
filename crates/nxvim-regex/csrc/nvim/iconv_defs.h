// nxvim-regex shim for nvim/iconv_defs.h: no iconv in the vendored engine;
// vimconv_T is never used for actual conversion by regexp.c.
#pragma once

typedef void *iconv_t;
