// nxvim-regex host shim API.
//
// Everything the vendored vim regexp engine needs from the surrounding
// editor flows through this interface: buffer lines, cursor/Visual state,
// marks, options, interrupts and error reporting. The Rust side of the crate
// is the only intended consumer.
//
// Thread safety: the engine keeps global state (upstream `rex`), so all
// nxre_*/vim_* calls must be externally serialized. The Rust wrapper holds a
// global mutex.
#pragma once

#include <stdbool.h>
#include <stdint.h>

#include "nvim/buffer_defs.h"
#include "nvim/pos_defs.h"

#ifdef __cplusplus
extern "C" {
#endif

/// Looks up mark `name` ('a'..'z' etc.) in the current buffer. Returns false
/// when the mark is not set. Registered per-process.
typedef bool (*nxre_mark_lookup_fn)(void *userdata, int name, linenr_T *lnum, colnr_T *col);

/// Allocates a buffer handle for matching. `get_line`/`userdata` provide
/// lines 1..line_count (see nxre_get_line_fn); the buffer starts with vim's
/// default 'iskeyword' and 'tabstop'.
buf_T *nxre_buf_new(nxre_get_line_fn get_line, void *userdata, linenr_T line_count);
void nxre_buf_free(buf_T *buf);
void nxre_buf_set_line_count(buf_T *buf, linenr_T line_count);

/// Rebuilds the keyword table from an 'iskeyword'-format option string.
/// Returns false (with the error retrievable via nxre_take_last_error) on a
/// malformed option string.
bool nxre_buf_set_iskeyword(buf_T *buf, const char *iskeyword);
void nxre_buf_set_tabstop(buf_T *buf, int64_t tabstop);

/// Allocates a window handle (cursor state for \%# / \%V assertions).
win_T *nxre_win_new(buf_T *buf);
void nxre_win_free(win_T *win);
void nxre_win_set_cursor(win_T *win, linenr_T lnum, colnr_T col);

/// Records the last Visual selection on the buffer, for \%V. `mode` is the
/// vim mode character ('v', 'V', or Ctrl-V = 0x16).
void nxre_buf_set_visual(buf_T *buf, linenr_T start_lnum, colnr_T start_col, linenr_T end_lnum,
                         colnr_T end_col, int mode);

/// Makes buf/win current: the engine reads curbuf/curwin for the context
/// assertions. Also (re)initializes the global character tables. Must be
/// called before vim_regcomp()/vim_regexec*().
void nxre_set_current(buf_T *buf, win_T *win);

/// Mark provider for the \%'m assertions; pass NULL to clear (every mark
/// then reads as "not set", which makes \%'m fail to match — vim's own
/// behavior for unset marks).
void nxre_set_mark_provider(nxre_mark_lookup_fn lookup, void *userdata);

/// Interrupt flag (vim's got_int): set from any thread to abort a running
/// match; the engine polls it via fast_breakcheck().
void nxre_set_interrupt(bool value);

/// Returns and clears the last error message reported by the engine via
/// emsg()/semsg()/iemsg(), or NULL if none was reported. The pointer is
/// valid until the next engine call.
const char *nxre_take_last_error(void);

/// 'regexpengine' (0 = automatic, 1 = backtracking, 2 = NFA) and
/// 'ignorecase'-independent engine options.
void nxre_set_regexpengine(int64_t engine);

/// Builds a proftime_T deadline `ms` milliseconds from now, for
/// vim_regexec_multi()'s time-limit parameter.
uint64_t nxre_profile_setlimit(int64_t ms);

#ifdef __cplusplus
}
#endif
