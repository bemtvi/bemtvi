// nxvim-regex shim for nvim/os/input.h: interrupt check only.
#pragma once

// Polls the host interrupt flag (sets got_int). See shim/nxre_shim.c.
void fast_breakcheck(void);
