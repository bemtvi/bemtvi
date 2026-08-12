// bemtvi-regex shim for nvim/strings.h: declarations for the string helpers
// vendored in csrc/nvim/strings.c (extracted subset).
#pragma once

#include <stdbool.h>
#include <stddef.h>

#include "nvim/os/os_defs.h"
#include "nvim/types_defs.h"

#define STRLEN_LITERAL(s) (sizeof(s) - 1)

typedef struct {
  int key;        ///< the key
  char *value;    ///< the value string
  size_t length;  ///< length of the value string
} keyvalue_T;

#define KEYVALUE_ENTRY(k, v) { (k), (v), STRLEN_LITERAL(v) }

int cmp_keyvalue_value_n(const void *a, const void *b);
char *vim_strchr(const char *string, int c);
char *xstrnsave(const char *str, size_t len);
char *vim_strsave_escaped(const char *string, const char *esc_chars);
char *vim_strsave_escaped_ext(const char *string, const char *esc_chars, char cc, bool bsl);
