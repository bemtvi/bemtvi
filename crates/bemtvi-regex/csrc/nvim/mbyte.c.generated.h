#define DEFINE_FUNC_ATTRIBUTES
#include "nvim/func_attr.h"
#undef DEFINE_FUNC_ATTRIBUTES
static bool prop_is_emojilike(const utf8proc_property_t *prop);
static int utf_safe_read_char_adv(const char **s, size_t *n);
static bool always_break(int bc);
static bool intable(const struct interval *table, size_t n_items, int c) FUNC_ATTR_CONST;
static bool always_break_two(int bc1, int bc2);
#define DEFINE_EMPTY_ATTRIBUTES
#include "nvim/func_attr.h"  // IWYU pragma: export

