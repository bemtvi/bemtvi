#!/usr/bin/env python3
"""Generate the nxvim book's source from the repository.

Two pipelines feed the book, both run here so the published site cannot drift
from the code:

  1. Curated docs import — copies long-form docs/*.md into the book, rewriting
     repo-relative links to absolute GitHub URLs (so a single edit to the
     canonical doc updates the book).

  2. nx.* API reference — extracts every PUBLIC declaration from the Lua prelude
     (function nx.NS.name(args) / nx.NS.name = function(args); nx._private
     excluded) together with the doc-comment block above it, one page per
     top-level namespace.

Finally it renders src/SUMMARY.md from src/SUMMARY.template.md, replacing the
{{API_REFERENCE}} marker with the generated namespace list.

Run from anywhere:  python3 book/gen/generate.py
Stdlib only; no third-party dependencies. Fails loud on missing inputs.
"""

import os
import re
import sys

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
GEN_DIR = os.path.dirname(os.path.abspath(__file__))
BOOK_DIR = os.path.dirname(GEN_DIR)
REPO_ROOT = os.path.dirname(BOOK_DIR)
SRC_DIR = os.path.join(BOOK_DIR, "src")
PRELUDE_DIR = os.path.join(REPO_ROOT, "crates", "nxvim-lua", "src", "prelude")

GH_BLOB = "https://github.com/davidrios/nxvim/blob/main"
GH_RAW = "https://raw.githubusercontent.com/davidrios/nxvim/main"


def die(msg):
    sys.stderr.write("generate.py: error: %s\n" % msg)
    sys.exit(1)


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


# ---------------------------------------------------------------------------
# 1. Curated docs import (with link rewriting)
# ---------------------------------------------------------------------------
# (source doc relative to repo root) -> (book page relative to SRC_DIR)
IMPORTS = [
    ("docs/features.md", "features/index.md"),
    ("docs/features/multicursor.md", "features/multicursor.md"),
    ("docs/features/docks.md", "features/docks.md"),
    ("docs/architecture.md", "architecture/overview.md"),
    ("docs/plugin-authoring.md", "plugins/authoring.md"),
    ("docs/autocmd-events.md", "plugins/autocmd-events.md"),
    ("docs/known-approximations.md", "appendix/known-approximations.md"),
    ("docs/verifying-downloads.md", "appendix/verifying-downloads.md"),
]

# (repo-relative source doc) -> (book page relative to SRC_DIR), for resolving
# cross-doc links to in-book pages instead of GitHub.
IMPORT_MAP = {src: dest for src, dest in IMPORTS}

LINK_RE = re.compile(r"(!?)\[([^\]]*)\]\(([^)]+)\)")
FENCE_RE = re.compile(r"^\s*(```|~~~)")


def resolve_target(target, src_dir_rel):
    """Classify and resolve one Markdown link target.

    src_dir_rel is the source doc's directory relative to the repo root, used as
    the base for resolving repo-relative links. Returns (kind, value, suffix):
      ("keep", literal, "")        — external / mailto / pure anchor; emit as-is
      ("repo", repo_rel_path, suf) — a repo-relative path (+ #anchor/?query suffix)
    """
    # External, anchors, mailto: leave untouched.
    if target.startswith(("http://", "https://", "#", "mailto:", "//")):
        return "keep", target, ""
    # Split off any #anchor / ?query suffix.
    suffix = ""
    for sep in ("#", "?"):
        i = target.find(sep)
        if i != -1:
            suffix = target[i:] + suffix
            target = target[:i]
    if target == "":
        return "keep", "#" + suffix.lstrip("#"), ""  # pure anchor
    # Resolve relative to the source doc's directory, normalize to repo-root rel.
    repo_rel = os.path.normpath(os.path.join(src_dir_rel, target))
    repo_rel = repo_rel.replace(os.sep, "/")
    return "repo", repo_rel, suffix


def rewrite_links(text, src_path_rel, dest_rel):
    """Rewrite a doc's links for the book.

    A link to another *imported* doc becomes an in-book relative link (so the
    page is navigable inside the book); any other repo-relative link is rewritten
    to an absolute GitHub URL. dest_rel is this page's path relative to SRC_DIR,
    used as the base for the in-book relative links.
    """
    src_dir_rel = os.path.dirname(src_path_rel)
    dest_dir = os.path.dirname(dest_rel)
    out_lines = []
    in_fence = False
    for line in text.split("\n"):
        if FENCE_RE.match(line):
            in_fence = not in_fence
            out_lines.append(line)
            continue
        if in_fence:
            out_lines.append(line)
            continue

        def repl(m):
            bang, label, target = m.group(1), m.group(2), m.group(3)
            kind, value, suffix = resolve_target(target.strip(), src_dir_rel)
            if kind == "keep":
                return "%s[%s](%s)" % (bang, label, value)
            # In-book link when the target doc is itself imported (not an image).
            if bang == "" and value in IMPORT_MAP:
                rel = os.path.relpath(IMPORT_MAP[value], dest_dir or ".")
                rel = rel.replace(os.sep, "/")
                return "[%s](%s)" % (label, rel + suffix)
            base = GH_RAW if bang == "!" else GH_BLOB
            return "%s[%s](%s/%s)" % (bang, label, base, value + suffix)

        out_lines.append(LINK_RE.sub(repl, line))
    return "\n".join(out_lines)


def import_docs():
    banner = (
        "<!-- GENERATED from %s by book/gen/generate.py. Do not edit here;"
        " edit the source doc. -->\n\n"
    )
    for src_rel, dest_rel in IMPORTS:
        src_abs = os.path.join(REPO_ROOT, src_rel)
        if not os.path.isfile(src_abs):
            die("curated source doc missing: %s" % src_rel)
        with open(src_abs, encoding="utf-8") as f:
            text = f.read()
        rewritten = rewrite_links(text, src_rel, dest_rel)
        write(os.path.join(SRC_DIR, dest_rel), (banner % src_rel) + rewritten)
        print("  imported %-44s -> src/%s" % (src_rel, dest_rel))


# ---------------------------------------------------------------------------
# 2. nx.* API reference extraction
# ---------------------------------------------------------------------------
DECL_RE = re.compile(
    r"^\s*(?:function\s+(nx\.[A-Za-z0-9_.]+)\s*\(([^)]*)\)"
    r"|(nx\.[A-Za-z0-9_.]+)\s*=\s*function\s*\(([^)]*)\))"
)
SEP_RE = re.compile(r"^\s*--+\s*-{3,}")  # `-- ----- section -----` separators
CODE_SPAN_RE = re.compile(r"(`+)(.*?)\1", re.DOTALL)


def escape_angles_outside_code(text):
    """Escape `<`/`>` in prose so vim key-notation (`<c-e>`, `<tab>`) survives.

    mdBook's Markdown parser otherwise reads them as (unclosed) HTML tags and
    drops them from the rendered page. Angle brackets inside `inline code`
    spans are already literal, so they are left untouched.
    """
    out = []
    last = 0
    for m in CODE_SPAN_RE.finditer(text):
        out.append(text[last:m.start()].replace("<", "&lt;").replace(">", "&gt;"))
        out.append(m.group(0))
        last = m.end()
    out.append(text[last:].replace("<", "&lt;").replace(">", "&gt;"))
    return "".join(out)


def is_private(dotted):
    return "._" in dotted or dotted.startswith("nx._")


def ns_title(ns):
    return "nx" if ns == "nx" else "nx.%s" % ns


def ns_page(ns):
    return "nx.md" if ns == "nx" else "nx.%s.md" % ns


def doc_above(lines, idx):
    """Collect the contiguous `--` comment block immediately above line idx."""
    block = []
    i = idx - 1
    while i >= 0:
        stripped = lines[i].strip()
        if stripped.startswith("--"):
            block.append(lines[i])
            i -= 1
        else:
            break
    block.reverse()
    # Drop separator/banner lines (`-- ----- foo -----`).
    block = [b for b in block if not SEP_RE.match(b)]
    cleaned = []
    for b in block:
        s = b.strip()
        s = s[2:]  # strip leading '--'
        if s.startswith(" "):
            s = s[1:]
        cleaned.append(s)
    # Trim leading/trailing blank comment lines.
    while cleaned and cleaned[0] == "":
        cleaned.pop(0)
    while cleaned and cleaned[-1] == "":
        cleaned.pop()
    return "\n".join(cleaned)


def extract_api():
    if not os.path.isdir(PRELUDE_DIR):
        die("prelude dir not found: %s" % PRELUDE_DIR)
    # namespace -> list of (name, args, doc); insertion order preserved.
    namespaces = {}
    seen = set()
    files = sorted(f for f in os.listdir(PRELUDE_DIR) if f.endswith(".lua"))
    for fname in files:
        with open(os.path.join(PRELUDE_DIR, fname), encoding="utf-8") as f:
            lines = f.read().split("\n")
        for i, line in enumerate(lines):
            m = DECL_RE.match(line)
            if not m:
                continue
            name = m.group(1) or m.group(3)
            args = m.group(2) if m.group(1) else m.group(4)
            if is_private(name) or name in seen:
                continue
            seen.add(name)
            parts = name[len("nx.") :].split(".")
            ns = "nx" if len(parts) == 1 else parts[0]
            doc = doc_above(lines, i)
            namespaces.setdefault(ns, []).append((name, args.strip(), doc, fname))

    if not namespaces:
        die("extracted zero nx.* declarations — extraction is broken")

    total = sum(len(v) for v in namespaces.values())
    # Order: the top-level `nx` page first, then namespaces alphabetically.
    ordered = sorted(namespaces.keys(), key=lambda n: (n != "nx", n))

    for ns in ordered:
        entries = namespaces[ns]
        out = ["# `%s`\n" % ns_title(ns)]
        if ns == "nx":
            out.append("Top-level `nx.*` functions (those without a sub-namespace).\n")
        out.append(
            "<!-- GENERATED from crates/nxvim-lua/src/prelude/ by"
            " book/gen/generate.py. Do not edit. -->\n"
        )
        for name, args, doc, fname in entries:
            out.append("## `%s(%s)`\n" % (name, args))
            if doc:
                out.append(escape_angles_outside_code(doc) + "\n")
            else:
                out.append("_No documentation comment in the prelude._\n")
            out.append(
                "<sub>Defined in [`%s`](%s/crates/nxvim-lua/src/prelude/%s).</sub>\n"
                % (fname, GH_BLOB, fname)
            )
        write(os.path.join(SRC_DIR, "api", ns_page(ns)), "\n".join(out))

    # Reference index page.
    idx = [
        "# nx.* API Reference\n",
        "The public `nx.*` Lua API, **extracted directly from the prelude**",
        "(`crates/nxvim-lua/src/prelude/*.lua`) by `book/gen/generate.py`. Every",
        "entry is a public declaration plus its doc-comment; private `nx._*`",
        "internals are excluded. This is the canonical surface per",
        "[ADR 0002](%s/docs/decisions/0002-native-plugin-system.md).\n" % GH_BLOB,
        "| Namespace | Functions |",
        "| --------- | --------- |",
    ]
    for ns in ordered:
        idx.append(
            "| [`%s`](%s) | %d |" % (ns_title(ns), ns_page(ns), len(namespaces[ns]))
        )
    idx.append("\n_%d functions across %d namespaces._" % (total, len(ordered)))
    write(os.path.join(SRC_DIR, "api", "index.md"), "\n".join(idx) + "\n")

    print(
        "  extracted %d nx.* functions across %d namespaces"
        % (total, len(ordered))
    )
    return ordered


# ---------------------------------------------------------------------------
# 3. Render SUMMARY.md from template
# ---------------------------------------------------------------------------
def render_summary(namespaces):
    template_path = os.path.join(SRC_DIR, "SUMMARY.template.md")
    if not os.path.isfile(template_path):
        die("missing %s" % template_path)
    with open(template_path, encoding="utf-8") as f:
        template = f.read()
    if "{{API_REFERENCE}}" not in template:
        die("SUMMARY.template.md has no {{API_REFERENCE}} marker")
    lines = []
    for ns in namespaces:
        lines.append("- [%s](api/%s)" % (ns_title(ns), ns_page(ns)))
    summary = template.replace("{{API_REFERENCE}}", "\n".join(lines))
    write(os.path.join(SRC_DIR, "SUMMARY.md"), summary)
    print("  rendered src/SUMMARY.md (%d API pages)" % len(namespaces))


def main():
    print("Generating the nxvim book from source...")
    print("Importing curated docs:")
    import_docs()
    print("Extracting nx.* API reference:")
    namespaces = extract_api()
    print("Rendering table of contents:")
    render_summary(namespaces)
    print("Done.")


if __name__ == "__main__":
    main()
