#!/usr/bin/env python3
"""Extract named top-level function definitions (and named top-level
data/table definitions) from a neovim C source file.

Used to build the csrc/nvim subset files (mbyte.c, charset.c, strings.c)
from vendor/neovim. Functions are matched by name on a definition whose
signature starts at column 0; the body is copied through the closing
brace at column 0. Table/variable definitions are matched by "name[" or
"name =" on a column-0 line and copied through the line ending in ";".

Usage: extract-subset.py <upstream.c> <name>...
Writes the extracted definitions to stdout in source order.
Exits 1 naming any symbol it could not find (fail loud).
"""
import re
import sys


def main() -> int:
    path, names = sys.argv[1], set(sys.argv[2:])
    lines = open(path).read().split("\n")
    found: dict[str, tuple[int, int]] = {}  # name -> (start, end) line idx
    i = 0
    while i < len(lines):
        line = lines[i]
        # candidate function definition: signature line(s) at column 0
        m = re.match(r"[A-Za-z_][A-Za-z0-9_ *]*?\b([A-Za-z_][A-Za-z0-9_]*)\(", line)
        if m and not line.startswith((" ", "\t", "#", "//", "typedef")) \
                and not re.match(r"(if|while|for|switch|return)\b", line):
            name = m.group(1)
            if name in names and name not in found:
                # back up over the doc comment block and any attribute lines
                start = i
                while start > 0 and (
                        lines[start - 1].lstrip().startswith(("///", "//", "*", "/*"))
                        or lines[start - 1].rstrip().endswith("*/")):
                    start -= 1
                # scan forward to the opening brace at col 0 context, then to
                # the closing brace at column 0
                j = i
                while j < len(lines) and not lines[j].startswith("{"):
                    if lines[j].rstrip().endswith(";") and "{" not in lines[j]:
                        break  # was just a declaration, not a definition
                    j += 1
                if j < len(lines) and lines[j].startswith("{"):
                    k = j + 1
                    while k < len(lines) and not lines[k].startswith("}"):
                        k += 1
                    found[name] = (start, k + 1)
                    i = k + 1
                    continue
        # candidate table / variable definition
        m = re.match(r"(?:static\s+)?(?:const\s+)?[A-Za-z_][A-Za-z0-9_ *]*?"
                     r"\b([A-Za-z_][A-Za-z0-9_]*)\s*(\[[^\]]*\])?\s*=", line)
        if m and not line.startswith((" ", "\t", "#", "//")):
            name = m.group(1)
            if name in names and name not in found:
                k = i
                while k < len(lines) and not lines[k].rstrip().endswith(";"):
                    k += 1
                found[name] = (i, k + 1)
                i = k + 1
                continue
        i += 1

    missing = names - set(found)
    if missing:
        print(f"error: not found in {path}: {', '.join(sorted(missing))}",
              file=sys.stderr)
        return 1
    for name, (s, e) in sorted(found.items(), key=lambda kv: kv[1]):
        print("\n".join(lines[s:e]))
        print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
