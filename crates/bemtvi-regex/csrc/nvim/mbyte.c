// bemtvi-regex: extracted subset of nvim/mbyte.c (multibyte/UTF-8 handling).
//
// Contains only the functions the vendored regexp engine (and this subset
// itself) needs, extracted with extract-subset.py — see that script and
// gen-headers.sh for the regeneration procedure. Unicode properties come
// from the vendored utf8proc, exactly as upstream. arabic_combine() and
// arabic_maycombine() are extracted from nvim/arabic.c (used by
// utf_composinglike()).

#include <assert.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <utf8proc.h>
#include <wctype.h>

#include "nvim/ascii_defs.h"
#include "nvim/buffer_defs.h"
#include "nvim/charset.h"
#include "nvim/globals.h"
#include "nvim/macros_defs.h"
#include "nvim/mbyte.h"
#include "nvim/mbyte_defs.h"
#include "nvim/memory.h"
#include "nvim/option_vars.h"
#include "nvim/pos_defs.h"
#include "nvim/types_defs.h"
#include "nvim/vim_defs.h"


// Character range table type used by utf_printable() (upstream mbyte.c).
struct interval {
  int first;
  int last;
};

// Arabic codepoints used by arabic_maycombine() (upstream arabic.c).
enum {
  a_ALEF_MADDA = 0x0622,
  a_ALEF_HAMZA_ABOVE = 0x0623,
  a_ALEF_HAMZA_BELOW = 0x0625,
  a_ALEF = 0x0627,
  a_LAM = 0x0644,
};

#include "mbyte.c.generated.h"

const uint8_t utf8len_tab[] = {
  // ?1 ?2 ?3 ?4 ?5 ?6 ?7 ?8 ?9 ?A ?B ?C ?D ?E ?F
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 0?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 1?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 2?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 3?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 4?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 5?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 6?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 7?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 8?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 9?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // A?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // B?
  2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // C?
  2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // D?
  3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,  // E?
  4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 1, 1,  // F?
};

/// Get class of pointer:
/// 0 for blank or NUL
/// 1 for punctuation
/// 2 for an alphanumeric word character
/// >2 for other word characters, including CJK and emoji
int mb_get_class(const char *p)
  FUNC_ATTR_PURE
{
  return mb_get_class_tab(p, curbuf->b_chartab);
}

int mb_get_class_tab(const char *p, const uint64_t *const chartab)
  FUNC_ATTR_PURE
{
  if (MB_BYTE2LEN((uint8_t)p[0]) == 1) {
    if (p[0] == NUL || ascii_iswhite(p[0])) {
      return 0;
    }
    if (vim_iswordc_tab((uint8_t)p[0], chartab)) {
      return 2;
    }
    return 1;
  }
  return utf_class_tab(utf_ptr2char(p), chartab);
}

static bool prop_is_emojilike(const utf8proc_property_t *prop)
{
  return prop->boundclass == UTF8PROC_BOUNDCLASS_EXTENDED_PICTOGRAPHIC
         || prop->boundclass == UTF8PROC_BOUNDCLASS_REGIONAL_INDICATOR;
}

/// Convert a UTF-8 byte sequence to a character number.
/// Doesn't handle ascii! only multibyte and illegal sequences. ASCII (including NUL)
/// are treated like illegal sequences.
///
/// @param[in]  p      String to convert.
/// @param[in]  len    Length of the character in bytes, 0 or 1 if illegal.
///
/// @return Unicode codepoint. A negative value when the sequence is illegal (or
///         ASCII, including NUL).
int32_t utf_ptr2CharInfo_impl(uint8_t const *p, uintptr_t const len)
  FUNC_ATTR_PURE FUNC_ATTR_NONNULL_ALL FUNC_ATTR_WARN_UNUSED_RESULT
{
// uint8_t is a reminder for clang to use smaller cmp
#define CHECK \
  do { \
    if (EXPECT((uint8_t)(cur & 0xC0U) != 0x80U, false)) { \
      return -1; \
    } \
  } while (0)

  static uint32_t const corrections[] = {
    (1U << 31),  // invalid - set invalid bits (safe to add as first 2 bytes
    (1U << 31),  // won't affect highest bit in normal ret)
    -(0x80U + (0xC0U << 6)),  // multibyte - subtract added UTF8 bits (1..10xxx and 10xxx)
    -(0x80U + (0x80U << 6) + (0xE0U << 12)),
    -(0x80U + (0x80U << 6) + (0x80U << 12) + (0xF0U << 18)),
    -(0x80U + (0x80U << 6) + (0x80U << 12) + (0x80U << 18) + (0xF8U << 24)),
    -(0x80U + (0x80U << 6) + (0x80U << 12) + (0x80U << 18) + (0x80U << 24)),  // + (0xFCU << 30)
  };

  // len is 0-6, but declared uintptr_t to avoid zeroing out upper bits
  uint32_t const corr = corrections[len];
  uint8_t cur;

  // reading second byte unconditionally, safe for invalid
  // as it cannot be the last byte, not safe for ascii
  uint32_t code_point = ((uint32_t)p[0] << 6) + (cur = p[1]);
  CHECK;
  if ((uint32_t)len < 3) {
    goto ret;  // len == 0, 1, 2
  }

  code_point = (code_point << 6) + (cur = p[2]);
  CHECK;
  if ((uint32_t)len == 3) {
    goto ret;
  }

  code_point = (code_point << 6) + (cur = p[3]);
  CHECK;
  if ((uint32_t)len == 4) {
    goto ret;
  }

  code_point = (code_point << 6) + (cur = p[4]);
  CHECK;
  if ((uint32_t)len == 5) {
    goto ret;
  }

  code_point = (code_point << 6) + (cur = p[5]);
  CHECK;
  // len == 6

ret:
  return (int32_t)(code_point + corr);

#undef CHECK
}

/// Convert a UTF-8 byte sequence to a character number.
///
/// If the sequence is illegal or truncated by a NUL then the first byte is
/// returned.
/// For an overlong sequence this may return zero.
/// Does not include composing characters for obvious reasons.
///
/// @param[in]  p_in  String to convert.
///
/// @return Unicode codepoint or byte value.
int utf_ptr2char(const char *const p_in)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL
{
  uint8_t *p = (uint8_t *)p_in;

  uint32_t const v0 = p[0];
  if (EXPECT(v0 < 0x80U, true)) {  // Be quick for ASCII.
    return (int)v0;
  }

  const uint8_t len = utf8len_tab[v0];
  if (EXPECT(len < 2, false)) {
    return (int)v0;
  }

#define CHECK(v) \
  do { \
    if (EXPECT((uint8_t)((v) & 0xC0U) != 0x80U, false)) { \
      return (int)v0; \
    } \
  } while (0)
#define LEN_RETURN(len_v, result) \
  do { \
    if (len == (len_v)) { \
      return (int)(result); \
    } \
  } while (0)
#define S(s) ((uint32_t)0x80U << (s))

  uint32_t const v1 = p[1];
  CHECK(v1);
  LEN_RETURN(2, (v0 << 6) + v1 - ((0xC0U << 6) + S(0)));

  uint32_t const v2 = p[2];
  CHECK(v2);
  LEN_RETURN(3, (v0 << 12) + (v1 << 6) + v2 - ((0xE0U << 12) + S(6) + S(0)));

  uint32_t const v3 = p[3];
  CHECK(v3);
  LEN_RETURN(4, (v0 << 18) + (v1 << 12) + (v2 << 6) + v3
             - ((0xF0U << 18) + S(12) + S(6) + S(0)));

  uint32_t const v4 = p[4];
  CHECK(v4);
  LEN_RETURN(5, (v0 << 24) + (v1 << 18) + (v2 << 12) + (v3 << 6) + v4
             - ((0xF8U << 24) + S(18) + S(12) + S(6) + S(0)));

  uint32_t const v5 = p[5];
  CHECK(v5);
  // len == 6
  return (int)((v0 << 30) + (v1 << 24) + (v2 << 18) + (v3 << 12) + (v4 << 6) + v5
               // - (0xFCU << 30)
               - (S(24) + S(18) + S(12) + S(6) + S(0)));

#undef S
#undef CHECK
#undef LEN_RETURN
}

// Convert a UTF-8 byte sequence to a wide character.
// String is assumed to be terminated by NUL or after "n" bytes, whichever
// comes first.
// The function is safe in the sense that it never accesses memory beyond the
// first "n" bytes of "s".
//
// On success, returns decoded codepoint, advances "s" to the beginning of
// next character and decreases "n" accordingly.
//
// If end of string was reached, returns 0 and, if "n" > 0, advances "s" past
// NUL byte.
//
// If byte sequence is illegal or incomplete, returns -1 and does not advance
// "s".
static int utf_safe_read_char_adv(const char **s, size_t *n)
{
  if (*n == 0) {  // end of buffer
    return 0;
  }

  uint8_t k = utf8len_tab_zero[(uint8_t)(**s)];

  if (k == 1) {
    // ASCII character or NUL
    (*n)--;
    return (uint8_t)(*(*s)++);
  }

  if (k <= *n) {
    // We have a multibyte sequence and it isn't truncated by buffer
    // limits so utf_ptr2char() is safe to use. Or the first byte is
    // illegal (k=0), and it's also safe to use utf_ptr2char().
    int c = utf_ptr2char(*s);

    // On failure, utf_ptr2char() returns the first byte, so here we
    // check equality with the first byte. The only non-ASCII character
    // which equals the first byte of its own UTF-8 representation is
    // U+00C3 (UTF-8: 0xC3 0x83), so need to check that special case too.
    // It's safe even if n=1, else we would have k=2 > n.
    if (c != (int)((uint8_t)(**s)) || (c == 0xC3 && (uint8_t)(*s)[1] == 0x83)) {
      // byte sequence was successfully decoded
      *s += k;
      *n -= k;
      return c;
    }
  }

  // byte sequence is incomplete or illegal
  return -1;
}

// Get character at **pp and advance *pp to the next character.
// Note: composing characters are skipped!
int mb_ptr2char_adv(const char **const pp)
{
  int c = utf_ptr2char(*pp);
  *pp += utfc_ptr2len(*pp);
  return c;
}

/// Check if the character pointed to by "p2" is a composing character when it
/// comes after "p1".
///
/// We use the definition in UAX#29 as implemented by utf8proc with the following
/// exceptions:
///
/// - ASCII chars always begin a new cluster. This is a long assumed invariant
///   in the code base and very useful for performance (we can exit early for ASCII
///   all over the place, branch predictor go brrr in ASCII-only text).
///   As of Unicode 15.1 this will only break BOUNDCLASS_UREPEND followed by ASCII,
///   which should be exceedingly rare (these PREPEND chars are expected to be
///   followed by multibyte chars within the same script family)
///
/// - When 'arabicshape' is active, some pairs of arabic letters "ab" is replaced with
///   "c" taking one single cell, which behaves like a cluster.
///
/// @param "state" should be set to GRAPHEME_STATE_INIT before first call
///        it is allowed to be null, but will then not handle some longer
///        sequences, like ZWJ based emoji
bool utf_composinglike(const char *p1, const char *p2, GraphemeState *state)
  FUNC_ATTR_NONNULL_ARG(1, 2)
{
  if ((uint8_t)(*p2) < 128) {
    return false;
  }

  int first = utf_ptr2char(p1);
  int second = utf_ptr2char(p2);

  if (!utf8proc_grapheme_break_stateful(first, second, state)) {
    return true;
  }

  return arabic_combine(first, second);
}

/// Get the length of a UTF-8 byte sequence representing a single codepoint
///
/// @param[in]  p  UTF-8 string.
///
/// @return Sequence length, 0 for empty string and 1 for non-UTF-8 byte
///         sequence.
int utf_ptr2len(const char *const p_in)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL
{
  uint8_t *p = (uint8_t *)p_in;
  if (*p == NUL) {
    return 0;
  }
  const int len = utf8len_tab[*p];
  for (int i = 1; i < len; i++) {
    if ((p[i] & 0xc0) != 0x80) {
      return 1;
    }
  }
  return len;
}

// Get the length of UTF-8 byte sequence "p[size]".  Does not include any
// following composing characters.
// Returns 1 for "".
// Returns 1 for an illegal byte sequence (also in incomplete byte seq.).
// Returns number > "size" for an incomplete byte sequence.
// Never returns zero.
int utf_ptr2len_len(const char *p, int size)
{
  int m;

  int len = utf8len_tab[(uint8_t)(*p)];
  if (len == 1) {
    return 1;           // NUL, ascii or illegal lead byte
  }
  if (len > size) {
    m = size;           // incomplete byte sequence.
  } else {
    m = len;
  }
  for (int i = 1; i < m; i++) {
    if ((p[i] & 0xc0) != 0x80) {
      return 1;
    }
  }
  return len;
}

/// Return the number of bytes occupied by a UTF-8 character in a string.
/// This includes following composing characters.
/// Returns zero for NUL.
int utfc_ptr2len(const char *const p)
  FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL
{
  uint8_t b0 = (uint8_t)(*p);

  if (b0 == NUL) {
    return 0;
  }
  if (b0 < 0x80 && (uint8_t)p[1] < 0x80) {  // be quick for ASCII
    return 1;
  }

  // Skip over first UTF-8 char, stopping at a NUL byte.
  int len = utf_ptr2len(p);

  // Check for illegal byte.
  if (len == 1 && b0 >= 0x80) {
    return 1;
  }

  // Check for composing characters.
  int prevlen = 0;
  GraphemeState state = GRAPHEME_STATE_INIT;
  while (true) {
    if ((uint8_t)p[len] < 0x80 || !utf_composinglike(p + prevlen, p + len, &state)) {
      return len;
    }

    // Skip over composing char.
    prevlen = len;
    len += utf_ptr2len(p + len);
  }
}

/// Return the number of bytes the UTF-8 encoding of the character at "p[size]"
/// takes.  This includes following composing characters.
/// Returns 0 for an empty string.
/// Returns 1 for an illegal char or an incomplete byte sequence.
int utfc_ptr2len_len(const char *p, int size)
{
  if (size < 1 || *p == NUL) {
    return 0;
  }
  if ((uint8_t)p[0] < 0x80 && (size == 1 || (uint8_t)p[1] < 0x80)) {  // be quick for ASCII
    return 1;
  }

  // Skip over first UTF-8 char, stopping at a NUL byte.
  int len = utf_ptr2len_len(p, size);

  // Check for illegal byte and incomplete byte sequence.
  if ((len == 1 && (uint8_t)p[0] >= 0x80) || len > size) {
    return 1;
  }

  // Check for composing characters.  We can only display a limited amount, but
  // skip all of them (otherwise the cursor would get stuck).
  int prevlen = 0;
  GraphemeState state = GRAPHEME_STATE_INIT;
  while (len < size) {
    if ((uint8_t)p[len] < 0x80) {
      break;
    }

    // Next character length should not go beyond size to ensure that
    // utf_composinglike(...) does not read beyond size.
    int len_next_char = utf_ptr2len_len(p + len, size - len);
    if (len_next_char > size - len) {
      break;
    }

    if (!utf_composinglike(p + prevlen, p + len, &state)) {
      break;
    }

    // Skip over composing char
    prevlen = len;
    len += len_next_char;
  }
  return len;
}

/// Determine how many bytes certain unicode codepoint will occupy
int utf_char2len(const int c)
{
  if (c < 0x80) {
    return 1;
  } else if (c < 0x800) {
    return 2;
  } else if (c < 0x10000) {
    return 3;
  } else if (c < 0x200000) {
    return 4;
  } else if (c < 0x4000000) {
    return 5;
  } else {
    return 6;
  }
}

/// Convert Unicode character to UTF-8 string
///
/// @param c         character to convert to UTF-8 string in \p buf
/// @param[out] buf  UTF-8 string generated from \p c, does not add \0
///                  must have room for at least 6 bytes
/// @return Number of bytes (1-6).
int utf_char2bytes(const int c, char *const buf)
{
  if (c < 0x80) {  // 7 bits
    buf[0] = (char)c;
    return 1;
  } else if (c < 0x800) {  // 11 bits
    buf[0] = (char)(0xc0 + ((unsigned)c >> 6));
    buf[1] = (char)(0x80 + ((unsigned)c & 0x3f));
    return 2;
  } else if (c < 0x10000) {  // 16 bits
    buf[0] = (char)(0xe0 + ((unsigned)c >> 12));
    buf[1] = (char)(0x80 + (((unsigned)c >> 6) & 0x3f));
    buf[2] = (char)(0x80 + ((unsigned)c & 0x3f));
    return 3;
  } else if (c < 0x200000) {  // 21 bits
    buf[0] = (char)(0xf0 + ((unsigned)c >> 18));
    buf[1] = (char)(0x80 + (((unsigned)c >> 12) & 0x3f));
    buf[2] = (char)(0x80 + (((unsigned)c >> 6) & 0x3f));
    buf[3] = (char)(0x80 + ((unsigned)c & 0x3f));
    return 4;
  } else if (c < 0x4000000) {  // 26 bits
    buf[0] = (char)(0xf8 + ((unsigned)c >> 24));
    buf[1] = (char)(0x80 + (((unsigned)c >> 18) & 0x3f));
    buf[2] = (char)(0x80 + (((unsigned)c >> 12) & 0x3f));
    buf[3] = (char)(0x80 + (((unsigned)c >> 6) & 0x3f));
    buf[4] = (char)(0x80 + ((unsigned)c & 0x3f));
    return 5;
  } else {  // 31 bits
    buf[0] = (char)(0xfc + ((unsigned)c >> 30));
    buf[1] = (char)(0x80 + (((unsigned)c >> 24) & 0x3f));
    buf[2] = (char)(0x80 + (((unsigned)c >> 18) & 0x3f));
    buf[3] = (char)(0x80 + (((unsigned)c >> 12) & 0x3f));
    buf[4] = (char)(0x80 + (((unsigned)c >> 6) & 0x3f));
    buf[5] = (char)(0x80 + ((unsigned)c & 0x3f));
    return 6;
  }
}

/// Return true if "c" is a legacy composing UTF-8 character.
///
/// This is deprecated in favour of utf_composinglike() which uses the modern
/// stateful algorithm to determine grapheme clusters. Still available
/// to support some legacy code which hasn't been refactored yet.
///
/// To check if a char would combine with a preceding space, use
/// utf_iscomposing_first() instead.
///
/// Based on code from Markus Kuhn.
/// Returns false for negative values.
bool utf_iscomposing_legacy(int c)
{
  const utf8proc_property_t *prop = utf8proc_get_property(c);
  return prop->category == UTF8PROC_CATEGORY_MN || prop->category == UTF8PROC_CATEGORY_ME;
}

// Get class of a Unicode character.
// 0: white space
// 1: punctuation
// 2 or bigger: some class of word character.
int utf_class(const int c)
{
  return utf_class_tab(c, curbuf->b_chartab);
}

int utf_class_tab(const int c, const uint64_t *const chartab)
  FUNC_ATTR_PURE
{
  // sorted list of non-overlapping intervals
  static struct clinterval {
    unsigned first;
    unsigned last;
    unsigned cls;
  } classes[] = {
    { 0x037e, 0x037e, 1 },              // Greek question mark
    { 0x0387, 0x0387, 1 },              // Greek ano teleia
    { 0x055a, 0x055f, 1 },              // Armenian punctuation
    { 0x0589, 0x0589, 1 },              // Armenian full stop
    { 0x05be, 0x05be, 1 },
    { 0x05c0, 0x05c0, 1 },
    { 0x05c3, 0x05c3, 1 },
    { 0x05f3, 0x05f4, 1 },
    { 0x060c, 0x060c, 1 },
    { 0x061b, 0x061b, 1 },
    { 0x061f, 0x061f, 1 },
    { 0x066a, 0x066d, 1 },
    { 0x06d4, 0x06d4, 1 },
    { 0x0700, 0x070d, 1 },              // Syriac punctuation
    { 0x0964, 0x0965, 1 },
    { 0x0970, 0x0970, 1 },
    { 0x0df4, 0x0df4, 1 },
    { 0x0e4f, 0x0e4f, 1 },
    { 0x0e5a, 0x0e5b, 1 },
    { 0x0f04, 0x0f12, 1 },
    { 0x0f3a, 0x0f3d, 1 },
    { 0x0f85, 0x0f85, 1 },
    { 0x104a, 0x104f, 1 },              // Myanmar punctuation
    { 0x10fb, 0x10fb, 1 },              // Georgian punctuation
    { 0x1361, 0x1368, 1 },              // Ethiopic punctuation
    { 0x166d, 0x166e, 1 },              // Canadian Syl. punctuation
    { 0x1680, 0x1680, 0 },
    { 0x169b, 0x169c, 1 },
    { 0x16eb, 0x16ed, 1 },
    { 0x1735, 0x1736, 1 },
    { 0x17d4, 0x17dc, 1 },              // Khmer punctuation
    { 0x1800, 0x180a, 1 },              // Mongolian punctuation
    { 0x2000, 0x200b, 0 },              // spaces
    { 0x200c, 0x2027, 1 },              // punctuation and symbols
    { 0x2028, 0x2029, 0 },
    { 0x202a, 0x202e, 1 },              // punctuation and symbols
    { 0x202f, 0x202f, 0 },
    { 0x2030, 0x205e, 1 },              // punctuation and symbols
    { 0x205f, 0x205f, 0 },
    { 0x2060, 0x206f, 1 },              // punctuation and symbols
    { 0x2070, 0x207f, 0x2070 },         // superscript
    { 0x2080, 0x2094, 0x2080 },         // subscript
    { 0x20a0, 0x27ff, 1 },              // all kinds of symbols
    { 0x2800, 0x28ff, 0x2800 },         // braille
    { 0x2900, 0x2998, 1 },              // arrows, brackets, etc.
    { 0x29d8, 0x29db, 1 },
    { 0x29fc, 0x29fd, 1 },
    { 0x2e00, 0x2e7f, 1 },              // supplemental punctuation
    { 0x3000, 0x3000, 0 },              // ideographic space
    { 0x3001, 0x3020, 1 },              // ideographic punctuation
    { 0x3030, 0x3030, 1 },
    { 0x303d, 0x303d, 1 },
    { 0x3040, 0x309f, 0x3040 },         // Hiragana
    { 0x30a0, 0x30ff, 0x30a0 },         // Katakana
    { 0x3300, 0x9fff, 0x4e00 },         // CJK Ideographs
    { 0xac00, 0xd7a3, 0xac00 },         // Hangul Syllables
    { 0xf900, 0xfaff, 0x4e00 },         // CJK Ideographs
    { 0xfd3e, 0xfd3f, 1 },
    { 0xfe30, 0xfe6b, 1 },              // punctuation forms
    { 0xff00, 0xff0f, 1 },              // half/fullwidth ASCII
    { 0xff1a, 0xff20, 1 },              // half/fullwidth ASCII
    { 0xff3b, 0xff40, 1 },              // half/fullwidth ASCII
    { 0xff5b, 0xff65, 1 },              // half/fullwidth ASCII
    { 0x1d000, 0x1d24f, 1 },            // Musical notation
    { 0x1d400, 0x1d7ff, 1 },            // Mathematical Alphanumeric Symbols
    { 0x1f000, 0x1f2ff, 1 },            // Game pieces; enclosed characters
    { 0x1f300, 0x1f9ff, 1 },            // Many symbol blocks
    { 0x20000, 0x2a6df, 0x4e00 },       // CJK Ideographs
    { 0x2a700, 0x2b73f, 0x4e00 },       // CJK Ideographs
    { 0x2b740, 0x2b81f, 0x4e00 },       // CJK Ideographs
    { 0x2f800, 0x2fa1f, 0x4e00 },       // CJK Ideographs
  };
  int bot = 0;
  int top = ARRAY_SIZE(classes) - 1;

  // First quick check for Latin1 characters, use 'iskeyword'.
  if (c < 0x100) {
    if (c == ' ' || c == '\t' || c == NUL || c == 0xa0) {
      return 0;             // blank
    }
    if (vim_iswordc_tab(c, chartab)) {
      return 2;             // word character
    }
    return 1;               // punctuation
  }

  const utf8proc_property_t *prop = utf8proc_get_property(c);
  // emoji
  if (prop_is_emojilike(prop)) {
    return 3;
  }

  // binary search in table
  while (top >= bot) {
    int mid = (bot + top) / 2;
    if (classes[mid].last < (unsigned)c) {
      bot = mid + 1;
    } else if (classes[mid].first > (unsigned)c) {
      top = mid - 1;
    } else {
      return (int)classes[mid].cls;
    }
  }

  // most other characters are "word" characters
  return 2;
}

// Return the folded-case equivalent of "a", which is a UCS-4 character.  Uses
// full case folding.
int utf_fold(int a)
{
  if (a < 0x80) {
    // be fast for ASCII
    return a >= 0x41 && a <= 0x5a ? a + 32 : a;
  }

  // TODO(dundargoc): utf8proc only does full case folding, which breaks some tests. This is a
  // temporary workaround to circumvent failing tests.
  //
  // (0xdf) ß == ss in full casefolding. Using this however breaks the vim spell tests and the error
  // E763 is thrown. This is due to the test spells relying on the vim spell files.
  //
  // (0x130) İ == i̇ in full casefolding.
  if (a == 0xdf || a == 0x130) {
    return a;
  }

  utf8proc_int32_t result[1];

  utf8proc_ssize_t res = utf8proc_decompose_char(a, result, 1, UTF8PROC_CASEFOLD, NULL);

  return (res == 1) ? result[0] : a;
}

/// Return the upper-case equivalent of "a", which is a UCS-4 character.  Use
/// simple case folding.
int mb_toupper(int a)
{
  // If 'casemap' contains "keepascii" use ASCII style toupper().
  if (a < 128 && (cmp_flags & kOptCmpFlagKeepascii)) {
    return TOUPPER_ASC(a);
  }

  if (!(cmp_flags & kOptCmpFlagInternal)) {
    return (int)towupper((wint_t)a);
  }

  // For characters below 128 use locale sensitive toupper().
  if (a < 128) {
    return TOUPPER_LOC(a);
  }

  return utf8proc_toupper(a);
}

bool mb_islower(int a)
{
  return mb_toupper(a) != a;
}

/// Return the lower-case equivalent of "a", which is a UCS-4 character.  Use
/// simple case folding.
int mb_tolower(int a)
{
  // If 'casemap' contains "keepascii" use ASCII style tolower().
  if (a < 128 && (cmp_flags & kOptCmpFlagKeepascii)) {
    return TOLOWER_ASC(a);
  }

  if (!(cmp_flags & kOptCmpFlagInternal)) {
    return (int)towlower((wint_t)a);
  }

  // For characters below 128 use locale sensitive tolower().
  if (a < 128) {
    return TOLOWER_LOC(a);
  }

  return utf8proc_tolower(a);
}

bool mb_isupper(int a)
{
  return mb_tolower(a) != a;
}

int utf_strnicmp(const char *s1, const char *s2, size_t n1, size_t n2)
{
  int c1, c2;
  char buffer[6];

  while (true) {
    c1 = utf_safe_read_char_adv(&s1, &n1);
    c2 = utf_safe_read_char_adv(&s2, &n2);

    if (c1 <= 0 || c2 <= 0) {
      break;
    }

    if (c1 == c2) {
      continue;
    }

    int cdiff = utf_fold(c1) - utf_fold(c2);
    if (cdiff != 0) {
      return cdiff;
    }
  }

  // some string ended or has an incomplete/illegal character sequence

  if (c1 == 0 || c2 == 0) {
    // some string ended. shorter string is smaller
    if (c1 == 0 && c2 == 0) {
      return 0;
    }
    return c1 == 0 ? -1 : 1;
  }

  // Continue with bytewise comparison to produce some result that
  // would make comparison operations involving this function transitive.
  //
  // If only one string had an error, comparison should be made with
  // folded version of the other string. In this case it is enough
  // to fold just one character to determine the result of comparison.

  if (c1 != -1 && c2 == -1) {
    n1 = (size_t)utf_char2bytes(utf_fold(c1), buffer);
    s1 = buffer;
  } else if (c2 != -1 && c1 == -1) {
    n2 = (size_t)utf_char2bytes(utf_fold(c2), buffer);
    s2 = buffer;
  }

  while (n1 > 0 && n2 > 0 && *s1 != NUL && *s2 != NUL) {
    int cdiff = (int)((uint8_t)(*s1)) - (int)((uint8_t)(*s2));
    if (cdiff != 0) {
      return cdiff;
    }

    s1++;
    s2++;
    n1--;
    n2--;
  }

  if (n1 > 0 && *s1 == NUL) {
    n1 = 0;
  }
  if (n2 > 0 && *s2 == NUL) {
    n2 = 0;
  }

  if (n1 == 0 && n2 == 0) {
    return 0;
  }
  return n1 == 0 ? -1 : 1;
}

/// Version of strnicmp() that handles multi-byte characters.
/// Needed for Big5, Shift-JIS and UTF-8 encoding.  Other DBCS encodings can
/// probably use strnicmp(), because there are no ASCII characters in the
/// second byte.
///
/// @return  zero if s1 and s2 are equal (ignoring case), the difference between
///          two characters otherwise.
int mb_strnicmp(const char *s1, const char *s2, const size_t nn)
{
  return utf_strnicmp(s1, s2, nn, nn);
}

/// @return true if boundclass bc always starts a new cluster regardless of what's before
/// false negatives are allowed (perf cost, not correctness)
static bool always_break(int bc)
{
  return (bc == UTF8PROC_BOUNDCLASS_CONTROL);
}

/// Return offset from "p" to the start of a character, including composing characters.
/// "base" must be the start of the string, which must be NUL terminated.
/// If "p" points to the NUL at the end of the string return 0.
/// Returns 0 when already at the first byte of a character.
int utf_head_off(const char *base_in, const char *p_in)
{
  if ((uint8_t)(*p_in) < 0x80) {              // be quick for ASCII
    return 0;
  }

  const uint8_t *base = (uint8_t *)base_in;
  const uint8_t *p = (uint8_t *)p_in;

  const uint8_t *start = p;

  // move start to the first byte of this codepoint
  // might stop on a continuation byte if overlong, handled by utf_ptr2CharInfo_impl
  while (start > base && (*start & 0xc0) == 0x80 && (p - start) < 6) {
    start--;
  }

  const uint8_t last_len = utf8len_tab[*start];
  int32_t cur_code = utf_ptr2CharInfo_impl(start, (uintptr_t)last_len);
  if (cur_code < 0 || p - start >= last_len) {
    return 0;  // p must be part of an illegal sequence
  }
  const uint8_t * const safe_end = start + last_len;

  int cur_bc = utf8proc_get_property(cur_code)->boundclass;
  if (always_break(cur_bc) || start == base) {
    return (int)(p - start);
  }

  // backtrack to find the start of a cluster. we might go too far, checked in the next loop
  const uint8_t *cur_pos = start;
  const uint8_t *const p_start = start;

  while (true) {
    if (start[-1] == NUL) {
      break;
    }

    start--;
    if (*start < 0x80) {  // stop on ascii, we are done
      break;
    }

    while (start > base && (*start & 0xc0) == 0x80 && (cur_pos - start) < 6) {
      start--;
    }

    int prev_len = utf8len_tab[*start];
    int32_t prev_code = utf_ptr2CharInfo_impl(start, (uintptr_t)prev_len);
    if (prev_code < 0 || prev_len < cur_pos - start) {
      start = cur_pos;  // start at valid sequence after invalid bytes
      break;
    }

    int prev_bc = utf8proc_get_property(prev_code)->boundclass;
    if (always_break_two(prev_bc, cur_bc) && !arabic_combine(prev_code, cur_code)) {
      start = cur_pos;  // prev_code cannot be a part of this cluster
      break;
    } else if (start == base) {
      break;
    }
    cur_pos = start;
    cur_bc = prev_bc;
    cur_code = prev_code;
  }

  // hot path: we are already on the first codepoint of a sequence
  if (start == p_start && last_len > p - start) {
    return (int)(p - start);
  }

  const uint8_t *q = start;
  while (q < p) {
    // don't need to find end of cluster. once we reached the codepoint of p, we are done
    int len = utfc_ptr2len_len((const char *)q, (int)(safe_end - q));

    if (q + len > p) {
      return (int)(p - q);
    }

    q += len;
  }

  return 0;
}

/// Assumes caller already handles ascii. see `utfc_next`
StrCharInfo utfc_next_impl(StrCharInfo cur)
{
  int32_t prev_code = cur.chr.value;
  uint8_t *next = (uint8_t *)(cur.ptr + cur.chr.len);
  GraphemeState state = GRAPHEME_STATE_INIT;
  assert(*next >= 0x80);

  while (true) {
    uint8_t const next_len = utf8len_tab[*next];
    int32_t const next_code = utf_ptr2CharInfo_impl(next, (uintptr_t)next_len);
    if (!utf_iscomposing(prev_code, next_code, &state)) {
      return (StrCharInfo){
        .ptr = (char *)next,
        .chr = (CharInfo){ .value = next_code, .len = (next_code < 0 ? 1 : next_len) },
      };
    }

    prev_code = next_code;
    next += next_len;
    if (EXPECT(*next < 0x80U, true)) {
      return (StrCharInfo){
        .ptr = (char *)next,
        .chr = (CharInfo){ .value = *next, .len = 1 },
      };
    }
  }
}

// utf_printable: both the SSE2 and scalar variants, with their #if guard
// (extracted as a block since extract-subset.py is guard-unaware).
#ifdef __SSE2__

# include <emmintrin.h>

// Return true for characters that can be displayed in a normal way.
// Only for characters of 0x100 and above!
bool utf_printable(int c)
  FUNC_ATTR_CONST
{
  if (c < 0x180B || c > 0xFFFF) {
    return c != 0x70F;
  }

# define L(v) ((int16_t)((v) - 1))  // lower bound (exclusive)
# define H(v) ((int16_t)(v))  // upper bound (inclusive)

  // Boundaries of unprintable characters.
  // Some values are negative when converted to int16_t.
  // Ranges must not wrap around when converted to int16_t.
  __m128i const lo = _mm_setr_epi16(L(0x180b), L(0x200b), L(0x202a), L(0x2060),
                                    L(0xd800), L(0xfeff), L(0xfff9), L(0xfffe));

  __m128i const hi = _mm_setr_epi16(H(0x180e), H(0x200f), H(0x202e), H(0x206f),
                                    H(0xdfff), H(0xfeff), H(0xfffb), H(0xffff));

# undef L
# undef H

  __m128i value = _mm_set1_epi16((int16_t)c);

  // Using _mm_cmplt_epi16() is less optimal, since it would require
  // swapping operands (sse2 only has cmpgt instruction),
  // and only the second operand can be a memory location.

  // Character is printable when it is above/below both bounds of each range
  // (corresponding bits in both masks are equal).
  return _mm_movemask_epi8(_mm_cmpgt_epi16(value, lo))
         == _mm_movemask_epi8(_mm_cmpgt_epi16(value, hi));
}

#else

// Return true if "c" is in "table".
static bool intable(const struct interval *table, size_t n_items, int c)
  FUNC_ATTR_CONST
{
  assert(n_items > 0);
  // first quick check for Latin1 etc. characters
  if (c < table[0].first) {
    return false;
  }

  assert(n_items <= SIZE_MAX / 2);
  // binary search in table
  size_t bot = 0;
  size_t top = n_items;
  do {
    size_t mid = (bot + top) >> 1;
    if (table[mid].last < c) {
      bot = mid + 1;
    } else if (table[mid].first > c) {
      top = mid;
    } else {
      return true;
    }
  } while (top > bot);
  return false;
}

// Return true for characters that can be displayed in a normal way.
// Only for characters of 0x100 and above!
bool utf_printable(int c)
  FUNC_ATTR_CONST
{
  // Sorted list of non-overlapping intervals.
  // 0xd800-0xdfff is reserved for UTF-16, actually illegal.
  static const struct interval nonprint[] = {
    { 0x070f, 0x070f }, { 0x180b, 0x180e }, { 0x200b, 0x200f }, { 0x202a, 0x202e },
    { 0x2060, 0x206f }, { 0xd800, 0xdfff }, { 0xfeff, 0xfeff }, { 0xfff9, 0xfffb },
    { 0xfffe, 0xffff }
  };

  return !intable(nonprint, ARRAY_SIZE(nonprint), c);
}

#endif

/// Check whether we are dealing with a character that could be regarded as an
/// Arabic combining character, need to check the character before this.
bool arabic_maycombine(int two)
  FUNC_ATTR_PURE
{
  if (p_arshape && !p_tbidi) {
    return two == a_ALEF_MADDA
           || two == a_ALEF_HAMZA_ABOVE
           || two == a_ALEF_HAMZA_BELOW
           || two == a_ALEF;
  }
  return false;
}

/// Check whether we are dealing with Arabic combining characters.
/// Returns false for negative values.
/// Note: these are NOT really composing characters!
///
/// @param one First character.
/// @param two Character just after "one".
bool arabic_combine(int one, int two)
  FUNC_ATTR_PURE
{
  if (one == a_LAM) {
    return arabic_maycombine(two);
  }
  return false;
}


/// same as utf_composinglike but operating on UCS-4 values
bool utf_iscomposing(int c1, int c2, GraphemeState *state)
{
  return (!utf8proc_grapheme_break_stateful(c1, c2, state)
          || arabic_combine(c1, c2));
}

/// @return true if bc2 always starts a cluster after bc1
/// false negatives are allowed (perf cost, not correctness)
static bool always_break_two(int bc1, int bc2)
{
  // don't check for UTF8PROC_BOUNDCLASS_CONTROL for bc2 as it either has been checked by
  // "always_break" on first iteration or when it was bc1 in the previous iteration
  return ((bc1 != UTF8PROC_BOUNDCLASS_PREPEND && bc2 == UTF8PROC_BOUNDCLASS_OTHER)
          || (bc1 >= UTF8PROC_BOUNDCLASS_CR && bc1 <= UTF8PROC_BOUNDCLASS_CONTROL)
          || (bc2 == UTF8PROC_BOUNDCLASS_EXTENDED_PICTOGRAPHIC
              && (bc1 == UTF8PROC_BOUNDCLASS_OTHER
                  || bc1 == UTF8PROC_BOUNDCLASS_EXTENDED_PICTOGRAPHIC)));
}

const uint8_t utf8len_tab_zero[] = {
  // ?1 ?2 ?3 ?4 ?5 ?6 ?7 ?8 ?9 ?A ?B ?C ?D ?E ?F
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 0?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 1?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 2?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 3?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 4?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 5?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 6?
  1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,  // 7?
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,  // 8?
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,  // 9?
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,  // A?
  0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,  // B?
  2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // C?
  2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,  // D?
  3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,  // E?
  4, 4, 4, 4, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 0, 0,  // F?
};

