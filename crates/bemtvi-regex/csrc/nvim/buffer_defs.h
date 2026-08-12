// bemtvi-regex shim for nvim/buffer_defs.h.
//
// The vendored regexp engine reaches the host editor only through the fields
// below. buf_T carries a line-provider callback (replacing memline) plus the
// few option/state fields the engine reads; win_T carries the cursor state
// used by the \%# and \%V assertions.
#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "nvim/pos_defs.h"

/// Mirrors the upstream memline field the engine reads (b_ml.ml_line_count).
typedef struct {
  linenr_T ml_line_count;
} btvre_memline_T;

/// Mirrors upstream visualinfo_T: the last Visual selection, for \%V.
typedef struct {
  pos_T vi_start;
  pos_T vi_end;
  int vi_mode;
  colnr_T vi_curswant;
} visualinfo_T;

typedef struct file_buffer buf_T;
typedef struct window_S win_T;

/// Returns line `lnum` (1-based) of the buffer as a NUL-terminated string,
/// storing its byte length in *len when len is non-NULL. Must not return
/// NULL for 1..ml_line_count.
///
/// Lifetime: the engine holds line pointers across further get_line calls
/// (it matches over several lines at once), so every returned pointer must
/// stay valid until the enclosing vim_regexec_multi()/vim_regsub_multi()
/// call returns — not merely until the next get_line call.
typedef const char *(*btvre_get_line_fn)(void *userdata, linenr_T lnum, colnr_T *len);

struct file_buffer {
  btvre_memline_T b_ml;       ///< line count lives here (upstream layout)
  visualinfo_T b_visual;     ///< last Visual range, for \%V
  uint64_t b_chartab[4];     ///< iskeyword bitmap, 256 bits (see charset.c)
  char *b_p_isk;             ///< 'iskeyword' option string (owned)
  bool b_p_lisp;             ///< 'lisp' option (keyword chars include '-')
  int64_t b_p_ts;            ///< 'tabstop', for virtual-column assertions

  btvre_get_line_fn btv_get_line;  ///< bemtvi line provider
  void *btv_ud;                   ///< passed to btv_get_line
};

struct window_S {
  buf_T *w_buffer;
  pos_T w_cursor;    ///< cursor position, for \%#
  colnr_T w_curswant;
};
