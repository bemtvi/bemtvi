// nxvim-regex shim for nvim/message.h: errors are routed to a host-registered
// sink (see shim/nxre_shim.c); msg_puts only feeds 'verbose' diagnostics.
#pragma once

#include <stdbool.h>

bool emsg(const char *s);
bool semsg(const char *fmt, ...);
void iemsg(const char *s);
void msg_puts(const char *s);
void internal_error(const char *where);
void siemsg(const char *fmt, ...);
void verbose_enter(void);
void verbose_leave(void);
