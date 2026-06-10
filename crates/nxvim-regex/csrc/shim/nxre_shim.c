// nxvim-regex host shim: implements every editor-side function the vendored
// engine calls, and instantiates the EXTERN globals (this TU defines EXTERN
// before including the headers that declare them).

// clock_gettime(CLOCK_MONOTONIC) under -std=c11 needs the POSIX feature
// macro on emscripten (and is harmless elsewhere).
#define _POSIX_C_SOURCE 200809L

#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <utf8proc.h>

#define EXTERN  // instantiate EXTERN/INIT globals in this TU
#include "nvim/errors.h"
#include "nvim/globals.h"
#include "nvim/option_vars.h"
#undef EXTERN

#include "nvim/ascii_defs.h"
#include "nvim/buffer_defs.h"
#include "nvim/charset.h"
#include "nvim/mark.h"
#include "nvim/mbyte.h"
#include "nvim/memline.h"
#include "nvim/memory.h"
#include "nvim/message.h"
#include "nvim/os/input.h"
#include "nvim/plines.h"
#include "nvim/profile.h"
#include "nvim/vim_defs.h"
#include "shim/nxre_shim.h"

// ----------------------------------------------------------------------------
// memory: allocate-or-abort, mirroring upstream xmalloc semantics

void *xmalloc(size_t size)
{
  void *p = malloc(size ? size : 1);
  if (p == NULL) {
    fprintf(stderr, "nxvim-regex: out of memory allocating %zu bytes\n", size);
    abort();
  }
  return p;
}

void *xcalloc(size_t count, size_t size)
{
  void *p = calloc(count ? count : 1, size ? size : 1);
  if (p == NULL) {
    fprintf(stderr, "nxvim-regex: out of memory allocating %zu*%zu bytes\n", count, size);
    abort();
  }
  return p;
}

void *xrealloc(void *ptr, size_t size)
{
  void *p = realloc(ptr, size ? size : 1);
  if (p == NULL) {
    fprintf(stderr, "nxvim-regex: out of memory reallocating %zu bytes\n", size);
    abort();
  }
  return p;
}

void xfree(void *ptr)
{
  free(ptr);
}

char *xstrdup(const char *str)
{
  size_t len = strlen(str) + 1;
  return memcpy(xmalloc(len), str, len);
}

void *xmallocz(size_t size)
{
  char *p = xmalloc(size + 1);
  p[size] = NUL;
  return p;
}

void *xmemdupz(const void *data, size_t len)
{
  char *p = xmalloc(len + 1);
  memcpy(p, data, len);
  p[len] = NUL;
  return p;
}

void *xmemcpyz(void *dst, const void *src, size_t len)
{
  memcpy(dst, src, len);
  ((char *)dst)[len] = NUL;
  return (char *)dst + len;
}

// ----------------------------------------------------------------------------
// message: errors are stored for the host to retrieve (and bump called_emsg,
// which the engine itself uses to detect parse errors)

enum { NXRE_ERRBUF = 512 };
static char last_error[NXRE_ERRBUF];
static bool have_error = false;

bool emsg(const char *s)
{
  snprintf(last_error, sizeof(last_error), "%s", s);
  have_error = true;
  called_emsg++;
  return true;
}

bool semsg(const char *fmt, ...)
{
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(last_error, sizeof(last_error), fmt, ap);
  va_end(ap);
  have_error = true;
  called_emsg++;
  return true;
}

void iemsg(const char *s)
{
  snprintf(last_error, sizeof(last_error), "nxvim-regex internal error: %s", s);
  have_error = true;
  called_emsg++;
}

void internal_error(const char *where)
{
  semsg("nxvim-regex internal error: %s", where);
}

void siemsg(const char *fmt, ...)
{
  char buf[NXRE_ERRBUF];
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(buf, sizeof(buf), fmt, ap);
  va_end(ap);
  iemsg(buf);
}

// verbose-mode message redirection is presentational in nvim; nxvim-regex
// routes verbose output straight to stderr in msg_puts(), so these are no-ops.
void verbose_enter(void)
{
}

void verbose_leave(void)
{
}

void msg_puts(const char *s)
{
  // Only reachable when the host raises p_verbose; engine-switch notices go
  // to stderr so they are never silently lost.
  fputs(s, stderr);
}

const char *nxre_take_last_error(void)
{
  if (!have_error) {
    return NULL;
  }
  have_error = false;
  return last_error;
}

// ----------------------------------------------------------------------------
// memline: buffer lines come from the host callback

static char *nxre_get_line_checked(buf_T *buf, linenr_T lnum, colnr_T *len)
{
  const char *line = buf->nx_get_line(buf->nx_ud, lnum, len);
  if (line == NULL) {
    // The engine never asks for lines outside 1..ml_line_count; a NULL here
    // is a host bug. Fail loud rather than fake an empty line.
    fprintf(stderr, "nxvim-regex: line provider returned NULL for line %" PRIdLINENR "\n", lnum);
    abort();
  }
  return (char *)line;
}

char *ml_get_buf(buf_T *buf, linenr_T lnum)
{
  return nxre_get_line_checked(buf, lnum, NULL);
}

colnr_T ml_get_buf_len(buf_T *buf, linenr_T lnum)
{
  colnr_T len = 0;
  nxre_get_line_checked(buf, lnum, &len);
  return len;
}

// ----------------------------------------------------------------------------
// marks

static nxre_mark_lookup_fn mark_lookup = NULL;
static void *mark_lookup_ud = NULL;

void nxre_set_mark_provider(nxre_mark_lookup_fn lookup, void *userdata)
{
  mark_lookup = lookup;
  mark_lookup_ud = userdata;
}

fmark_T *mark_get(buf_T *buf, win_T *win, fmark_T *fmp, MarkGet flag, int name)
{
  (void)buf;
  (void)win;
  (void)fmp;
  (void)flag;
  static fmark_T fm;
  fm.mark.lnum = 0;  // "not set" unless the provider says otherwise
  fm.mark.col = 0;
  fm.mark.coladd = 0;
  if (mark_lookup != NULL
      && mark_lookup(mark_lookup_ud, name, &fm.mark.lnum, &fm.mark.col)) {
    return &fm;
  }
  return &fm;  // lnum == 0 -> engine treats the mark as unset (NOMATCH)
}

// ----------------------------------------------------------------------------
// interrupts and time limits

void nxre_set_interrupt(bool value)
{
  got_int = value;
}

void fast_breakcheck(void)
{
  // got_int is set directly by the host (nxre_set_interrupt); nothing to poll.
}

static uint64_t monotonic_ns(void)
{
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return (uint64_t)ts.tv_sec * 1000000000u + (uint64_t)ts.tv_nsec;
}

uint64_t nxre_profile_setlimit(int64_t ms)
{
  return monotonic_ns() + (uint64_t)ms * 1000000u;
}

bool profile_passed_limit(proftime_T tm)
{
  return tm != 0 && monotonic_ns() > tm;
}

// ----------------------------------------------------------------------------
// virtual columns (\%v family): tabstop + utf8proc character widths.
//
// This approximates vim's charsize machinery: tabs expand against 'tabstop',
// printable-width comes from utf8proc_charwidth(), unprintable ASCII counts
// as ^X (2 cells). Divergences (e.g. 'vartabstop', <xx> display of latin1
// control bytes) are accepted and documented in the crate docs.

int win_linetabsize(win_T *wp, linenr_T lnum, char *line, colnr_T len)
{
  (void)lnum;
  int64_t ts = wp->w_buffer != NULL ? wp->w_buffer->b_p_ts : 8;
  if (ts <= 0) {
    ts = 8;
  }
  int vcol = 0;
  char *p = line;
  while (*p != NUL && (colnr_T)(p - line) < len) {
    int c = utf_ptr2char(p);
    if (c == TAB) {
      vcol += (int)(ts - (vcol % ts));
    } else if (c < 0x80 && !vim_isprintc(c)) {
      vcol += 2;  // ^X form
    } else {
      int w = utf8proc_charwidth((utf8proc_int32_t)c);
      vcol += w > 0 ? w : 0;
    }
    p += utfc_ptr2len(p);
  }
  return vcol;
}

/// Virtual column of a position (simplified getvcol/getvvcol: no 'virtualedit'
/// coladd handling beyond pass-through, same width model as win_linetabsize).
void getvvcol(win_T *wp, pos_T *pos, colnr_T *start, colnr_T *cursor, colnr_T *end, int flags)
{
  (void)flags;
  colnr_T col = 0;
  if (wp->w_buffer != NULL && pos->lnum >= 1
      && pos->lnum <= wp->w_buffer->b_ml.ml_line_count) {
    colnr_T len = 0;
    char *line = nxre_get_line_checked(wp->w_buffer, pos->lnum, &len);
    colnr_T upto = pos->col < len ? pos->col : len;
    col = (colnr_T)win_linetabsize(wp, pos->lnum, line, upto);
  }
  col += pos->coladd;
  if (start != NULL) {
    *start = col;
  }
  if (cursor != NULL) {
    *cursor = col;
  }
  if (end != NULL) {
    *end = col;
  }
}

// ----------------------------------------------------------------------------
// buffers, windows, options

// vim defaults (see :help 'isident' etc.); isfname is the unix default.
static char isk_default[] = "@,48-57,_,192-255";

buf_T *nxre_buf_new(nxre_get_line_fn get_line, void *userdata, linenr_T line_count)
{
  buf_T *buf = xcalloc(1, sizeof(buf_T));
  buf->b_ml.ml_line_count = line_count;
  buf->b_p_ts = 8;
  buf->b_p_isk = xstrdup(isk_default);
  buf->nx_get_line = get_line;
  buf->nx_ud = userdata;
  buf_init_chartab(buf, false);
  return buf;
}

void nxre_buf_free(buf_T *buf)
{
  if (buf == NULL) {
    return;
  }
  if (curbuf == buf) {
    curbuf = NULL;
  }
  xfree(buf->b_p_isk);
  xfree(buf);
}

void nxre_buf_set_line_count(buf_T *buf, linenr_T line_count)
{
  buf->b_ml.ml_line_count = line_count;
}

bool nxre_buf_set_iskeyword(buf_T *buf, const char *iskeyword)
{
  char *old = buf->b_p_isk;
  buf->b_p_isk = xstrdup(iskeyword);
  if (buf_init_chartab(buf, false) == FAIL) {
    emsg("nxvim-regex: invalid 'iskeyword' option string");
    xfree(buf->b_p_isk);
    buf->b_p_isk = old;
    buf_init_chartab(buf, false);
    return false;
  }
  xfree(old);
  return true;
}

void nxre_buf_set_tabstop(buf_T *buf, int64_t tabstop)
{
  buf->b_p_ts = tabstop;
}

win_T *nxre_win_new(buf_T *buf)
{
  win_T *win = xcalloc(1, sizeof(win_T));
  win->w_buffer = buf;
  win->w_cursor.lnum = 1;
  return win;
}

void nxre_win_set_cursor(win_T *win, linenr_T lnum, colnr_T col)
{
  win->w_cursor.lnum = lnum;
  win->w_cursor.col = col;
  win->w_cursor.coladd = 0;
}

void nxre_buf_set_visual(buf_T *buf, linenr_T start_lnum, colnr_T start_col, linenr_T end_lnum,
                         colnr_T end_col, int mode)
{
  buf->b_visual.vi_start.lnum = start_lnum;
  buf->b_visual.vi_start.col = start_col;
  buf->b_visual.vi_start.coladd = 0;
  buf->b_visual.vi_end.lnum = end_lnum;
  buf->b_visual.vi_end.col = end_col;
  buf->b_visual.vi_end.coladd = 0;
  buf->b_visual.vi_mode = mode;
  buf->b_visual.vi_curswant = 0;
  // The engine's \%V gate (reg_match_visual) requires the global VIsual to
  // be set even when reading b_visual — vim keeps the last selection's anchor
  // there after Visual mode exits. Mirror that.
  VIsual.lnum = start_lnum;
  VIsual.col = start_col;
  VIsual.coladd = 0;
  VIsual_active = false;
}

void nxre_win_free(win_T *win)
{
  if (curwin == win) {
    curwin = NULL;
  }
  xfree(win);
}

void nxre_set_current(buf_T *buf, win_T *win)
{
  curbuf = buf;
  curwin = win;
  // (Re)build the global chartab against this buffer's options.
  buf_init_chartab(buf, true);
}

void nxre_set_regexpengine(int64_t engine)
{
  p_re = engine;
}
