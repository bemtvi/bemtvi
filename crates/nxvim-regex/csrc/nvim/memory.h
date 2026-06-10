// nxvim-regex shim for nvim/memory.h: the allocation-or-abort family the
// vendored sources use, implemented in shim/nxre_shim.c.
#pragma once

#include <stddef.h>

#include "nvim/func_attr.h"
#include "nvim/macros_defs.h"

void *xmalloc(size_t size);
void *xcalloc(size_t count, size_t size);
void *xrealloc(void *ptr, size_t size);
void xfree(void *ptr);
char *xstrdup(const char *str);
void *xmallocz(size_t size);
void *xmemdupz(const void *data, size_t len);
void *xmemcpyz(void *dst, const void *src, size_t len);

#define CLEAR_FIELD(field)  memset(&(field), 0, sizeof(field))
#define STRCPY(d, s)        strcpy((char *)(d), (char *)(s))  // NOLINT(runtime/printf)

#define XFREE_CLEAR(ptr) \
  do { \
    void **ptr_ = (void **)&(ptr); \
    xfree(*ptr_); \
    *ptr_ = NULL; \
  } while (0)
