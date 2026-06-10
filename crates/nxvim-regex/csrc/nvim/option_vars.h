// nxvim-regex shim for nvim/option_vars.h: the option globals the engine
// reads, instantiated in shim/nxre_shim.c with vim's defaults and settable
// through the nxre_* API.
#pragma once

#include <stdint.h>

#include "nvim/macros_defs.h"

typedef int64_t OptInt;

#define MAX_MCO  6  // fixed value for 'maxcombine' (upstream option_vars.h)

// Flag enums for 'casemap' and 'display', matching the build-generated
// kOpt*Flag* values (2^(index-1) over the option's values list).
enum {
  kOptCmpFlagInternal = 0x01,
  kOptCmpFlagKeepascii = 0x02,
};
enum {
  kOptDyFlagLastline = 0x01,
  kOptDyFlagTruncate = 0x02,
  kOptDyFlagUhex = 0x04,
  kOptDyFlagMsgsep = 0x08,
};

// 'cpoptions' flag read by the regexp parser.
#define CPO_LITERAL 'l'  ///< take char classes literally

EXTERN char *p_cpo INIT( = "aABceFs");      ///< 'cpoptions'
EXTERN char *p_isi INIT( = "@,48-57,_,192-255");  ///< 'isident'
EXTERN char *p_isp INIT( = "@,161-255");          ///< 'isprint'
EXTERN char *p_isf INIT( = "@,48-57,/,.,-,_,+,,,#,$,%,~,=");  ///< 'isfname'
EXTERN char *p_sel INIT( = "inclusive");    ///< 'selection'
EXTERN OptInt p_re INIT( = 0);              ///< 'regexpengine': 0 = auto
EXTERN OptInt p_mmp INIT( = 1000000);       ///< 'maxmempattern' (KiB)
EXTERN OptInt p_verbose INIT( = 0);         ///< 'verbose'
EXTERN unsigned cmp_flags INIT( = kOptCmpFlagInternal | kOptCmpFlagKeepascii);  ///< 'casemap' default
EXTERN unsigned dy_flags INIT( = kOptDyFlagLastline);  ///< 'display' default
EXTERN int p_arshape INIT( = true);         ///< 'arabicshape'
EXTERN int p_tbidi INIT( = false);          ///< 'termbidi'
