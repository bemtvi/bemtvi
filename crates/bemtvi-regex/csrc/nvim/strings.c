// bemtvi-regex: extracted subset of nvim/strings.c (string helpers used by the
// vendored regexp engine), extracted with extract-subset.py.

#include <stdbool.h>
#include <stddef.h>
#include <string.h>

#include "nvim/ascii_defs.h"
#include "nvim/charset.h"
#include "nvim/macros_defs.h"
#include "nvim/mbyte.h"
#include "nvim/memory.h"
#include "nvim/strings.h"
#include "nvim/types_defs.h"
#include "nvim/vim_defs.h"

/// Copy up to `len` bytes of `string` into newly allocated memory and
/// terminate with a NUL. The allocated memory always has size `len + 1`, even
/// when `string` is shorter.
char *xstrnsave(const char *string, size_t len)
  FUNC_ATTR_NONNULL_RET FUNC_ATTR_MALLOC FUNC_ATTR_NONNULL_ALL
{
  return strncpy(xmallocz(len), string, len);  // NOLINT(runtime/printf)
}

// Same as vim_strsave(), but any characters found in esc_chars are preceded
// by a backslash.
char *vim_strsave_escaped(const char *string, const char *esc_chars)
  FUNC_ATTR_NONNULL_RET FUNC_ATTR_MALLOC FUNC_ATTR_NONNULL_ALL
{
  return vim_strsave_escaped_ext(string, esc_chars, '\\', false);
}

// Same as vim_strsave_escaped(), but when "bsl" is true also escape
// characters where rem_backslash() would remove the backslash.
// Escape the characters with "cc".
char *vim_strsave_escaped_ext(const char *string, const char *esc_chars, char cc, bool bsl)
  FUNC_ATTR_NONNULL_RET FUNC_ATTR_MALLOC FUNC_ATTR_NONNULL_ALL
{
  // First count the number of backslashes required.
  // Then allocate the memory and insert them.
  size_t length = 1;                    // count the trailing NUL
  for (const char *p = string; *p; p++) {
    const size_t l = (size_t)(utfc_ptr2len(p));
    if (l > 1) {
      length += l;                      // count a multibyte char
      p += l - 1;
      continue;
    }
    if (vim_strchr(esc_chars, (uint8_t)(*p)) != NULL || (bsl && rem_backslash(p))) {
      length++;                         // count a backslash
    }
    length++;                           // count an ordinary char
  }

  char *escaped_string = xmalloc(length);
  char *p2 = escaped_string;
  for (const char *p = string; *p; p++) {
    const size_t l = (size_t)(utfc_ptr2len(p));
    if (l > 1) {
      memcpy(p2, p, l);
      p2 += l;
      p += l - 1;                     // skip multibyte char
      continue;
    }
    if (vim_strchr(esc_chars, (uint8_t)(*p)) != NULL || (bsl && rem_backslash(p))) {
      *p2++ = cc;
    }
    *p2++ = *p;
  }
  *p2 = NUL;

  return escaped_string;
}

/// strchr() version which handles multibyte strings
///
/// @param[in]  string  String to search in.
/// @param[in]  c  Character to search for.
///
/// @return Pointer to the first byte of the found character in string or NULL
///         if it was not found or character is invalid. NUL character is never
///         found, use `strlen()` instead.
char *vim_strchr(const char *const string, const int c)
  FUNC_ATTR_NONNULL_ALL FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT
{
  if (c <= 0) {
    return NULL;
  } else if (c < 0x80) {
    // NOLINTNEXTLINE(*-casting): remove once CI uses glibc 2.43
    return (char *)strchr(string, c);
  } else {
    char u8char[MB_MAXBYTES + 1];
    const int len = utf_char2bytes(c, u8char);
    u8char[len] = NUL;
    // NOLINTNEXTLINE(*-casting): remove once CI uses glibc 2.43
    return (char *)strstr(string, u8char);
  }
}

/// compare two keyvalue_T structs by value with length
int cmp_keyvalue_value_n(const void *a, const void *b)
{
  keyvalue_T *kv1 = (keyvalue_T *)a;
  keyvalue_T *kv2 = (keyvalue_T *)b;

  return strncmp(kv1->value, kv2->value, MAX(kv1->length, kv2->length));
}

