#!/bin/sh
# A stand-in "compiler" for the quickfix tour: it prints gcc-style diagnostics
# against examples/quickfix/sample.c on stderr (so :make sees them the way it
# sees a real build's errors), then exits non-zero like a failed compile.
#
# Run from the repo root — :make invokes it via 'makeprg', and the relative
# `examples/quickfix/sample.c` paths in the output resolve against the CWD, so
# <CR> / :cnext open the real file.
f=examples/quickfix/sample.c
{
  echo "$f:9:18: error: expected ';' before '}' token"
  echo "$f:14:28: warning: 'totl' undeclared (first use in this function)"
  echo "$f:15:5: error: implicit declaration of function 'undeclared_helper'"
} >&2
exit 1
