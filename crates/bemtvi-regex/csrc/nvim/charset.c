// bemtvi-regex: extracted subset of nvim/charset.c (character classification).
//
// Contains the chartab machinery and the classification functions the
// vendored regexp engine uses, extracted with extract-subset.py. The chartab
// is built from the same option strings as upstream ('isident', 'isprint',
// 'isfname', buffer-local 'iskeyword'), which the host sets through the
// btvre_* API; defaults are vim's.

#include <assert.h>
#include <errno.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

#include "nvim/ascii_defs.h"
#include "nvim/buffer_defs.h"
#include "nvim/charset.h"
#include "nvim/globals.h"
#include "nvim/macros_defs.h"
#include "nvim/mbyte.h"
#include "nvim/memory.h"
#include "nvim/option_vars.h"
#include "nvim/pos_defs.h"
#include "nvim/strings.h"
#include "nvim/types_defs.h"
#include "nvim/vim_defs.h"

#include "charset.c.generated.h"

static bool chartab_initialized = false;

// b_chartab[] is an array with 256 bits, each bit representing one of the
// characters 0-255.
#define SET_CHARTAB(buf, c) \
  (buf)->b_chartab[(unsigned)(c) >> 6] |= (1ull << ((c) & 0x3f))
#define RESET_CHARTAB(buf, c) \
  (buf)->b_chartab[(unsigned)(c) >> 6] &= ~(1ull << ((c) & 0x3f))
#define GET_CHARTAB_TAB(chartab, c) \
  ((chartab)[(unsigned)(c) >> 6] & (1ull << ((c) & 0x3f)))

// Table used below, see init_chartab() for an explanation
static uint8_t g_chartab[256];

// Flags for g_chartab[].
#define CT_CELL_MASK  0x07  ///< mask: nr of display cells (1, 2 or 4)
#define CT_PRINT_CHAR 0x10  ///< flag: set for printable chars
#define CT_ID_CHAR    0x20  ///< flag: set for ID chars
#define CT_FNAME_CHAR 0x40  ///< flag: set for file name chars

/// Fill g_chartab[].  Also fills curbuf->b_chartab[] with flags for keyword
/// characters for current buffer.
///
/// Depends on the option settings 'iskeyword', 'isident', 'isfname',
/// 'isprint' and 'encoding'.
///
/// The index in g_chartab[] is the character when first byte is up to 0x80,
/// if the first byte is 0x80 and above it depends on further bytes.
///
/// The contents of g_chartab[]:
/// - The lower two bits, masked by CT_CELL_MASK, give the number of display
///   cells the character occupies (1 or 2).  Not valid for UTF-8 above 0x80.
/// - CT_PRINT_CHAR bit is set when the character is printable (no need to
///   translate the character before displaying it).  Note that only DBCS
///   characters can have 2 display cells and still be printable.
/// - CT_FNAME_CHAR bit is set when the character can be in a file name.
/// - CT_ID_CHAR bit is set when the character can be in an identifier.
///
/// @return FAIL if 'iskeyword', 'isident', 'isfname' or 'isprint' option has
/// an error, OK otherwise.
int init_chartab(void)
{
  return buf_init_chartab(curbuf, true);
}

/// Helper for init_chartab
///
/// @param global false: only set buf->b_chartab[]
///
/// @return FAIL if 'iskeyword', 'isident', 'isfname' or 'isprint' option has
/// an error, OK otherwise.
int buf_init_chartab(buf_T *buf, bool global)
{
  if (global) {
    // Set the default size for printable characters:
    // From <Space> to '~' is 1 (printable), others are 2 (not printable).
    // This also inits all 'isident' and 'isfname' flags to false.
    int c = 0;

    while (c < ' ') {
      g_chartab[c++] = (dy_flags & kOptDyFlagUhex) ? 4 : 2;
    }

    while (c <= '~') {
      g_chartab[c++] = 1 + CT_PRINT_CHAR;
    }

    while (c < 256) {
      if (c >= 0xa0) {
        // UTF-8: bytes 0xa0 - 0xff are printable (latin1)
        // Also assume that every multi-byte char is a filename character.
        g_chartab[c++] = (CT_PRINT_CHAR | CT_FNAME_CHAR) + 1;
      } else {
        // the rest is unprintable by default
        g_chartab[c++] = (dy_flags & kOptDyFlagUhex) ? 4 : 2;
      }
    }
  }

  // Init word char flags all to false
  CLEAR_FIELD(buf->b_chartab);

  // In lisp mode the '-' character is included in keywords.
  if (buf->b_p_lisp) {
    SET_CHARTAB(buf, '-');
  }

  // Walk through the 'isident', 'iskeyword', 'isfname' and 'isprint' options.
  for (int i = global ? 0 : 3; i <= 3; i++) {
    const char *p;
    if (i == 0) {
      // first round: 'isident'
      p = p_isi;
    } else if (i == 1) {
      // second round: 'isprint'
      p = p_isp;
    } else if (i == 2) {
      // third round: 'isfname'
      p = p_isf;
    } else {  // i == 3
      // fourth round: 'iskeyword'
      p = buf->b_p_isk;
    }
    if (parse_isopt(p, buf, false) == FAIL) {
      return FAIL;
    }
  }

  chartab_initialized = true;
  return OK;
}

/// Check that "c" is a keyword character:
/// Letters and characters from 'iskeyword' option for given buffer.
/// For multi-byte characters mb_get_class() is used (builtin rules).
///
/// @param  c    character to check
/// @param  buf  buffer whose keywords to use
bool vim_iswordc_buf(const int c, buf_T *const buf)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ARG(2)
{
  return vim_iswordc_tab(c, buf->b_chartab);
}

/// Just like vim_iswordc_buf() but uses a pointer to the (multi-byte)
/// character.
///
/// @param  p    pointer to the multi-byte character
/// @param  buf  buffer whose keywords to use
///
/// @return true if "p" points to a keyword character.
bool vim_iswordp_buf(const char *const p, buf_T *const buf)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL
{
  int c = (uint8_t)(*p);

  if (MB_BYTE2LEN(c) > 1) {
    c = utf_ptr2char(p);
  }
  return vim_iswordc_buf(c, buf);
}

/// Check that "c" is a valid file-name character as specified with the
/// 'isfname' option.
/// Assume characters above 0x100 are valid (multi-byte).
/// To be used for commands like "gf".
///
/// @param  c  character to check
bool vim_isfilec(int c)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT
{
  return c >= 0x100 || (c > 0 && (g_chartab[c] & CT_FNAME_CHAR));
}

/// Check that "c" is a printable character.
///
/// @param  c  character to check
bool vim_isprintc(int c)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT
{
  if (c >= 0x100) {
    return utf_printable(c);
  }
  return c > 0 && (g_chartab[c] & CT_PRINT_CHAR);
}

/// Gets a number from a string and skips over it.
///
/// @param[out]  pp  Pointer to a pointer to char.
///                  It will be advanced past the read number.
/// @param strict    Abort on overflow.
/// @param def       Default value, if parsing fails or overflow occurs.
///
/// @return Number read from the string, or `def` on parse failure or overflow.
intmax_t getdigits(char **pp, bool strict, intmax_t def)
{
  intmax_t number;
  int ok = try_getdigits(pp, &number);
  if (strict && !ok) {
    abort();
  }
  return ok ? number : def;
}

/// Gets an int number from a string.
///
/// @see getdigits
int getdigits_int(char **pp, bool strict, int def)
{
  intmax_t number = getdigits(pp, strict, def);
#if SIZEOF_INTMAX_T > SIZEOF_INT
  if (strict) {
    assert(number >= INT_MIN && number <= INT_MAX);
  } else if (!(number >= INT_MIN && number <= INT_MAX)) {
    return def;
  }
#endif
  return (int)number;
}

/// Return the value of a single hex character.
/// Only valid when the argument is '0' - '9', 'A' - 'F' or 'a' - 'f'.
///
/// @param c
///
/// @return The value of the hex character.
int hex2nr(int c)
  FUNC_ATTR_CONST
{
  if ((c >= 'a') && (c <= 'f')) {
    return c - 'a' + 10;
  }

  if ((c >= 'A') && (c <= 'F')) {
    return c - 'A' + 10;
  }
  return c - '0';
}

/// Check that "c" is a normal identifier character:
/// Letters and characters from the 'isident' option.
///
/// @param  c  character to check
bool vim_isIDc(int c)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT
{
  return c > 0 && c < 0x100 && (g_chartab[c] & CT_ID_CHAR);
}

/// @param only_check  if false: refill g_chartab[]
static int parse_isopt(const char *var, buf_T *buf, bool only_check)
{
  const char *p = var;

  // Parses the 'isident', 'iskeyword', 'isfname' and 'isprint' options.
  // Each option is a list of characters, character numbers or ranges,
  // separated by commas, e.g.: "200-210,x,#-178,-"
  while (*p) {
    bool tilde = false;
    bool do_isalpha = false;

    if (*p == '^' && p[1] != NUL) {
      tilde = true;
      p++;
    }

    int c;
    if (ascii_isdigit(*p)) {
      c = getdigits_int((char **)&p, true, 0);
    } else {
      c = mb_ptr2char_adv(&p);
    }
    int c2 = -1;

    if (*p == '-' && p[1] != NUL) {
      p++;

      if (ascii_isdigit(*p)) {
        c2 = getdigits_int((char **)&p, true, 0);
      } else {
        c2 = mb_ptr2char_adv(&p);
      }
    }

    if (c <= 0 || c >= 256 || (c2 < c && c2 != -1) || c2 >= 256
        || !(*p == NUL || *p == ',')) {
      return FAIL;
    }

    bool trail_comma = *p == ',';
    p = skip_to_option_part(p);
    if (trail_comma && *p == NUL) {
      // Trailing comma is not allowed.
      return FAIL;
    }

    if (only_check) {
      continue;
    }

    if (c2 == -1) {  // not a range
      // A single '@' (not "@-@"):
      // Decide on letters being ID/printable/keyword chars with
      // standard function isalpha(). This takes care of locale for
      // single-byte characters).
      if (c == '@') {
        do_isalpha = true;
        c = 1;
        c2 = 255;
      } else {
        c2 = c;
      }
    }

    while (c <= c2) {
      // Use the MB_ functions here, because isalpha() doesn't
      // work properly when 'encoding' is "latin1" and the locale is
      // "C".
      if (!do_isalpha
          || mb_islower(c)
          || mb_isupper(c)) {
        if (var == p_isi) {  // (re)set ID flag
          if (tilde) {
            g_chartab[c] &= (uint8_t) ~CT_ID_CHAR;
          } else {
            g_chartab[c] |= CT_ID_CHAR;
          }
        } else if (var == p_isp) {  // (re)set printable
          if (c < ' ' || c > '~') {
            if (tilde) {
              g_chartab[c] = (uint8_t)((g_chartab[c] & ~CT_CELL_MASK)
                                       + ((dy_flags & kOptDyFlagUhex) ? 4 : 2));
              g_chartab[c] &= (uint8_t) ~CT_PRINT_CHAR;
            } else {
              g_chartab[c] = (uint8_t)((g_chartab[c] & ~CT_CELL_MASK) + 1);
              g_chartab[c] |= CT_PRINT_CHAR;
            }
          }
        } else if (var == p_isf) {  // (re)set fname flag
          if (tilde) {
            g_chartab[c] &= (uint8_t) ~CT_FNAME_CHAR;
          } else {
            g_chartab[c] |= CT_FNAME_CHAR;
          }
        } else {  // (var == p_isk || var == buf->b_p_isk) (re)set keyword flag
          if (tilde) {
            RESET_CHARTAB(buf, c);
          } else {
            SET_CHARTAB(buf, c);
          }
        }
      }
      c++;
    }
  }

  return OK;
}

/// Check that "c" is a keyword character
/// Letters and characters from 'iskeyword' option for given buffer.
/// For multi-byte characters mb_get_class() is used (builtin rules).
///
/// @param[in]  c  Character to check.
/// @param[in]  chartab  Buffer chartab.
bool vim_iswordc_tab(const int c, const uint64_t *const chartab)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL
{
  return (c >= 0x100
          ? (utf_class_tab(c, chartab) >= 2)
          : (c > 0 && GET_CHARTAB_TAB(chartab, c) != 0));
}

/// Check that "str" starts with a backslash that should be removed.
/// For Windows this is only done when the character after the
/// backslash is not a normal file name character.
/// '$' is a valid file name character, we don't remove the backslash before
/// it.  This means it is not possible to use an environment variable after a
/// backslash.  "C:\$VIM\doc" is taken literally, only "$VIM\doc" works.
/// Although "\ name" is valid, the backslash in "Program\ files" must be
/// removed.  Assume a file name doesn't start with a space.
/// For multi-byte names, never remove a backslash before a non-ascii
/// character, assume that all multi-byte characters are valid file name
/// characters.
///
/// @param  str  file path string to check
bool rem_backslash(const char *str)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL
{
#ifdef BACKSLASH_IN_FILENAME
  return str[0] == '\\'
         && (uint8_t)str[1] < 0x80
         && (str[1] == ' '
             || (str[1] != NUL
                 && str[1] != '*'
                 && str[1] != '?'
                 && !vim_isfilec((uint8_t)str[1])));

#else
  return str[0] == '\\' && str[1] != NUL;
#endif
}

/// Gets a number from a string and skips over it, signalling overflow.
///
/// @param[out]  pp  A pointer to a pointer to char.
///                  It will be advanced past the read number.
/// @param[out]  nr  Number read from the string.
///
/// @return true on success, false on error/overflow
bool try_getdigits(char **pp, intmax_t *nr)
{
  errno = 0;
  *nr = strtoimax(*pp, pp, 10);
  if (errno == ERANGE && (*nr == INTMAX_MIN || *nr == INTMAX_MAX)) {
    return false;
  }
  return true;
}

/// Skip to next part of an option argument: skip space and comma
char *skip_to_option_part(const char *p)
{
  if (*p == ',') {
    p++;
  }
  while (*p == ' ') {
    p++;
  }
  return (char *)p;
}

