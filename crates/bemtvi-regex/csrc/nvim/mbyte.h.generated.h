#define DEFINE_FUNC_ATTRIBUTES
#include "nvim/func_attr.h"
#undef DEFINE_FUNC_ATTRIBUTES
#ifndef DLLEXPORT
#  ifdef MSWIN
#    define DLLEXPORT __declspec(dllexport)
#  else
#    define DLLEXPORT
#  endif
#endif
DLLEXPORT int mb_get_class(const char *p) FUNC_ATTR_PURE;
DLLEXPORT int mb_get_class_tab(const char *p, const uint64_t *const chartab) FUNC_ATTR_PURE;
DLLEXPORT int32_t utf_ptr2CharInfo_impl(uint8_t const *p, uintptr_t const len) FUNC_ATTR_PURE FUNC_ATTR_NONNULL_ALL FUNC_ATTR_WARN_UNUSED_RESULT;
DLLEXPORT int utf_ptr2char(const char *const p_in) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL;
DLLEXPORT int mb_ptr2char_adv(const char **const pp);
DLLEXPORT bool utf_composinglike(const char *p1, const char *p2, GraphemeState *state) FUNC_ATTR_NONNULL_ARG(1, 2);
DLLEXPORT int utf_ptr2len(const char *const p_in) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL;
DLLEXPORT int utf_ptr2len_len(const char *p, int size);
DLLEXPORT int utfc_ptr2len(const char *const p) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL;
DLLEXPORT int utfc_ptr2len_len(const char *p, int size);
DLLEXPORT int utf_char2len(const int c);
DLLEXPORT int utf_char2bytes(const int c, char *const buf);
DLLEXPORT bool utf_iscomposing_legacy(int c);
DLLEXPORT int utf_class(const int c);
DLLEXPORT int utf_class_tab(const int c, const uint64_t *const chartab) FUNC_ATTR_PURE;
DLLEXPORT int utf_fold(int a);
DLLEXPORT int mb_toupper(int a);
DLLEXPORT bool mb_islower(int a);
DLLEXPORT int mb_tolower(int a);
DLLEXPORT bool mb_isupper(int a);
DLLEXPORT int utf_strnicmp(const char *s1, const char *s2, size_t n1, size_t n2);
DLLEXPORT int mb_strnicmp(const char *s1, const char *s2, const size_t nn);
DLLEXPORT int utf_head_off(const char *base_in, const char *p_in);
DLLEXPORT StrCharInfo utfc_next_impl(StrCharInfo cur);
DLLEXPORT bool utf_printable(int c) FUNC_ATTR_CONST;
DLLEXPORT bool utf_printable(int c) FUNC_ATTR_CONST;
DLLEXPORT bool arabic_maycombine(int two) FUNC_ATTR_PURE;
DLLEXPORT bool arabic_combine(int one, int two) FUNC_ATTR_PURE;
DLLEXPORT bool utf_iscomposing(int c1, int c2, GraphemeState *state);
#include "nvim/func_attr.h"
