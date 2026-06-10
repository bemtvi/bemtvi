// nxvim-regex shim for nvim/globals.h: only the globals the vendored sources
// read. They are instantiated in shim/nxre_shim.c (which defines EXTERN) and
// set through the nxre_* API before matching.
#pragma once

#include <stdbool.h>

#include "nvim/buffer_defs.h"
#include "nvim/macros_defs.h"
#include "nvim/pos_defs.h"
#include "nvim/regexp_defs.h"

EXTERN buf_T *curbuf INIT( = NULL);
EXTERN win_T *curwin INIT( = NULL);

/// Set by the host (e.g. on Ctrl-C) to interrupt long-running matches.
EXTERN volatile int got_int INIT( = false);

/// Incremented by emsg(); the engine uses it to detect reported errors.
EXTERN int called_emsg INIT( = 0);

/// Set when vim_regcomp() called emsg() (upstream globals.h).
EXTERN bool rc_did_emsg INIT( = false);

// \z\( \) external submatch state for syntax-pattern matching (verbatim
// from upstream globals.h).
EXTERN int reg_do_extmatch INIT( = 0);       // Used when compiling regexp:
                                             // REX_SET to allow \z\(...\),
                                             // REX_USE to allow \z\1 et al.
// Used by vim_regexec(): strings for \z\1...\z\9
EXTERN reg_extmatch_T *re_extmatch_in INIT( = NULL);
// Set by vim_regexec() to store \z\(...\) matches
EXTERN reg_extmatch_T *re_extmatch_out INIT( = NULL);

// Visual mode state, read by the \%V assertion when matching in curbuf while
// Visual is active (otherwise curbuf->b_visual is used).
EXTERN pos_T VIsual;
EXTERN bool VIsual_active INIT( = false);
EXTERN int VIsual_mode INIT( = 'v');
