#define DEFINE_FUNC_ATTRIBUTES
#include "nvim/func_attr.h"
#undef DEFINE_FUNC_ATTRIBUTES
static inline bool utf_is_trail_byte(uint8_t const byte) FUNC_ATTR_CONST FUNC_ATTR_ALWAYS_INLINE;
static inline CharInfo utf_ptr2CharInfo(char const *const p_in) FUNC_ATTR_NONNULL_ALL FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_ALWAYS_INLINE;
static inline StrCharInfo utfc_next(StrCharInfo cur) FUNC_ATTR_NONNULL_ALL FUNC_ATTR_ALWAYS_INLINE FUNC_ATTR_PURE;
static inline StrCharInfo utf_ptr2StrCharInfo(char *ptr) FUNC_ATTR_NONNULL_ALL FUNC_ATTR_ALWAYS_INLINE FUNC_ATTR_PURE;
#define DEFINE_EMPTY_ATTRIBUTES
#include "nvim/func_attr.h"  // IWYU pragma: export

