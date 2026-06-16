// Portability shims for compiling the vendored neovim C subset under
// toolchains neovim itself never targets. Force-included by build.rs only
// where needed, so the vendored sources under csrc/nvim stay pristine across
// re-vendoring.
#pragma once

// MSVC has no POSIX <sys/types.h> `ssize_t`; map it onto the Win32 signed
// size type. regexp.c (reg_submatch) is the lone consumer.
#ifdef _MSC_VER
#include <BaseTsd.h>
typedef SSIZE_T ssize_t;
#endif
