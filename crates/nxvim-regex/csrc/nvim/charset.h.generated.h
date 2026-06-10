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
DLLEXPORT int init_chartab(void);
DLLEXPORT int buf_init_chartab(buf_T *buf, bool global);
DLLEXPORT bool vim_iswordc_buf(const int c, buf_T *const buf) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ARG(2);
DLLEXPORT bool vim_iswordp_buf(const char *const p, buf_T *const buf) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL;
DLLEXPORT bool vim_isfilec(int c) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT;
DLLEXPORT bool vim_isprintc(int c) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT;
DLLEXPORT intmax_t getdigits(char **pp, bool strict, intmax_t def);
DLLEXPORT int getdigits_int(char **pp, bool strict, int def);
DLLEXPORT int hex2nr(int c) FUNC_ATTR_CONST;
DLLEXPORT bool vim_isIDc(int c) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT;
DLLEXPORT bool vim_iswordc_tab(const int c, const uint64_t *const chartab) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL;
DLLEXPORT bool rem_backslash(const char *str) FUNC_ATTR_PURE FUNC_ATTR_WARN_UNUSED_RESULT FUNC_ATTR_NONNULL_ALL;
DLLEXPORT bool try_getdigits(char **pp, intmax_t *nr);
DLLEXPORT char *skip_to_option_part(const char *p);
#include "nvim/func_attr.h"
