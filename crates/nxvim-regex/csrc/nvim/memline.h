// nxvim-regex shim for nvim/memline.h. The engine fetches buffer lines only
// through these two functions; shim/nxre_shim.c forwards them to the buf_T
// line-provider callback.
#pragma once

#include "nvim/buffer_defs.h"
#include "nvim/pos_defs.h"

char *ml_get_buf(buf_T *buf, linenr_T lnum);
colnr_T ml_get_buf_len(buf_T *buf, linenr_T lnum);
