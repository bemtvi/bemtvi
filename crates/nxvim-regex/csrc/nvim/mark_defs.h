// nxvim-regex shim for nvim/mark_defs.h: only what the \%'m assertions use.
#pragma once

#include <stdbool.h>

#include "nvim/pos_defs.h"

typedef struct {
  pos_T mark;  ///< mark position; lnum <= 0 means "not set"
} fmark_T;

#include "nvim/func_attr.h"

/// Return true if position a is before (less than) position b. (Verbatim
/// from upstream mark_defs.h.)
static inline bool lt(pos_T a, pos_T b)
{
  if (a.lnum != b.lnum) {
    return a.lnum < b.lnum;
  } else if (a.col != b.col) {
    return a.col < b.col;
  } else {
    return a.coladd < b.coladd;
  }
}

static inline bool equalpos(pos_T a, pos_T b)
{
  return (a.lnum == b.lnum) && (a.col == b.col) && (a.coladd == b.coladd);
}

static inline bool ltoreq(pos_T a, pos_T b)
{
  return lt(a, b) || equalpos(a, b);
}

/// Options when getting a mark (subset of upstream MarkGet).
typedef enum {
  kMarkBufLocal,
} MarkGet;
