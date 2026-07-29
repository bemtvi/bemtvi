#!/usr/bin/env python3
"""Generate the nxvim book's source from the repository.

Two pipelines feed the book, both run here so the published site cannot drift
from the code:

  1. Curated docs import — copies long-form docs/*.md into the book, rewriting
     repo-relative links to absolute GitHub URLs (so a single edit to the
     canonical doc updates the book).

  2. nx.* API reference — extracts every PUBLIC declaration from the Lua prelude
     together with the doc-comment block above it, one page per top-level
     namespace (nx._private excluded). The prelude declares functions in four
     shapes, one collector each:
       * `function nx.NS.name(args)` / `nx.NS.name = function(args)`  (DECL_RE)
       * `nx.NS = { name = function(args) … }` table literals   (collect_table_literals)
       * a factory installing one verb table onto twin surfaces  (collect_surface_factories)
         — `nx.git` / `nx.git_local`
       * a module-local alias, `local M = nx.NS` + `function M.name()`
                                                          (collect_module_aliases)
     A COVERAGE GUARD then fails the build if any namespace the prelude creates
     produced no page, so a fifth shape cannot silently drop a whole module the
     way the alias shape hid `nx.plugins` and `nx.editorconfig`.

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
    ("docs/features/helix-mode.md", "features/helix-mode.md"),
    ("docs/features/multicursor.md", "features/multicursor.md"),
    ("docs/features/smooth-scrolling.md", "features/smooth-scrolling.md"),
    ("docs/features/image-previews.md", "features/image-previews.md"),
    ("docs/features/workspaces.md", "features/workspaces.md"),
    ("docs/features/ui-primitives.md", "features/ui-primitives.md"),
    ("docs/features/docks.md", "features/docks.md"),
    ("docs/features/picker.md", "features/picker.md"),
    ("docs/features/quickfix-dock-lists.md", "features/quickfix-dock-lists.md"),
    ("docs/browser-editor.md", "features/browser-editor.md"),
    ("docs/edit-host-split.md", "features/edit-host-split.md"),
    ("docs/architecture.md", "architecture/overview.md"),
    ("docs/recommended-plugins.md", "guide/recommended-plugins.md"),
    ("docs/nx-model.md", "plugins/nx-model.md"),
    ("docs/first-party-plugins.md", "plugins/first-party.md"),
    ("docs/plugin-authoring.md", "plugins/authoring.md"),
    ("docs/async.md", "plugins/async.md"),
    ("docs/plugin-testing.md", "plugins/testing.md"),
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
# `nx.NS = {` opening a namespace table literal whose fields are the public
# functions (e.g. `nx.fs_local = { exists = function(path) … }` in localseam.lua).
TABLE_OPEN_RE = re.compile(r"^\s*(nx\.[A-Za-z0-9_.]+)\s*=\s*\{\s*$")
FIELD_FN_RE = re.compile(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*function\s*\(([^)]*)\)")
# A factory that installs the SAME verb table onto several `nx.*` surfaces via a
# local parameter (e.g. `local function define(surface, bridge)` in git.lua, with
# `function surface.head(path)` methods, invoked as `define(nx.git, …)` /
# `define(nx.git_local, …)`). One verb table, two surfaces, no drift.
LOCAL_FACTORY_RE = re.compile(r"^\s*local\s+function\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(([^)]*)\)")
SURFACE_METHOD_RE = re.compile(
    r"^\s*function\s+([A-Za-z_][A-Za-z0-9_]*)\.([A-Za-z0-9_]+)\s*\(([^)]*)\)"
)
# A module-local ALIAS for a namespace: `local M = nx.plugins` at the top level, after
# which the file writes `function M.lock()` instead of `function nx.plugins.lock()`.
# Purely a spelling choice, but it hides the whole module from `DECL_RE` — which is how
# `nx.plugins` (28 public fns) and `nx.editorconfig` went undocumented for their entire
# existence. Only a bare `nx.<NS>` right-hand side counts (`local M = {}` is a plain
# table, not a namespace).
MODULE_ALIAS_RE = re.compile(r"^local\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(nx\.[A-Za-z0-9_]+)\s*$")
ALIAS_ASSIGN_RE = re.compile(
    r"^\s*([A-Za-z_][A-Za-z0-9_]*)\.([A-Za-z0-9_]+)\s*=\s*function\s*\(([^)]*)\)"
)
# Namespaces the prelude creates that legitimately expose NO functions, so the
# "every namespace is documented" guard below must not flag them. Keep this list tiny and
# justified — an entry here is a claim that the namespace is pure data.
NO_PUBLIC_API = {
    # `nx.g` is the global-variable table (the `vim.g` alias) — values, not functions.
    "g",
    # `nx.cmdline` holds only `nx.cmdline.actions[name] = fn`, a registry written by
    # callers (keymap.lua); it declares no functions of its own.
    "cmdline",
}
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


def _brace_delta(line):
    """Net `{`/`}` count on a line, ignoring a trailing `-- comment`."""
    code = line.split("--", 1)[0]
    return code.count("{") - code.count("}")


def collect_table_literals(lines):
    """Yield (name, args, decl_idx) for `nx.NS = { field = function(args) }` decls.

    Only fields at the table's top level (brace depth 1) count, so nested tables
    and inline `{ … }` values inside a field body are never mistaken for methods.
    """
    for i, line in enumerate(lines):
        m = TABLE_OPEN_RE.match(line)
        if not m:
            continue
        ns_name = m.group(1)
        depth = 1
        j = i + 1
        while j < len(lines) and depth > 0:
            fld = FIELD_FN_RE.match(lines[j])
            if depth == 1 and fld:
                yield ("%s.%s" % (ns_name, fld.group(1)), fld.group(2), j)
            depth += _brace_delta(lines[j])
            j += 1


def collect_surface_factories(lines):
    """Yield (name, args, decl_idx) for the twin-surface factory pattern.

    A `local function F(p1, …)` whose body adds `function p1.method(args)` and is
    later called as `F(nx.git, …)` / `F(nx.git_local, …)` installs one verb table
    onto every `nx.*` surface passed at p1's argument position. We emit each method
    under every such surface, so both twins are documented from the single source.
    """
    # param name -> (factory F, 0-based position of that param)
    surface_params = {}
    for line in lines:
        f = LOCAL_FACTORY_RE.match(line)
        if not f:
            continue
        params = [p.strip() for p in f.group(2).split(",") if p.strip()]
        for pos, pname in enumerate(params):
            surface_params[pname] = (f.group(1), pos)

    # For each factory, the `nx.*` surfaces bound to each param position across all
    # of its call sites: factory F -> { pos -> [nx.NS, …] }.
    factory_surfaces = {}
    for line in lines:
        for pname, (factory, pos) in surface_params.items():
            call = re.match(r"^\s*%s\s*\((.*)\)\s*$" % re.escape(factory), line)
            if not call:
                continue
            argv = [a.strip() for a in call.group(1).split(",")]
            if pos < len(argv) and re.match(r"^nx\.[A-Za-z0-9_]+$", argv[pos]):
                factory_surfaces.setdefault(factory, {}).setdefault(pos, []).append(argv[pos])

    for i, line in enumerate(lines):
        meth = SURFACE_METHOD_RE.match(line)
        if not meth:
            continue
        holder, name, args = meth.group(1), meth.group(2), meth.group(3)
        binding = surface_params.get(holder)
        if not binding:
            continue
        factory, pos = binding
        for surface in factory_surfaces.get(factory, {}).get(pos, []):
            yield ("%s.%s" % (surface, name), args, i)


def collect_module_aliases(lines):
    """Yield (name, args, decl_idx) for functions written through a module-local alias.

    A file may alias its namespace once (`local M = nx.plugins`) and then declare every
    public function as `function M.lock()`. That is invisible to `DECL_RE`, which only
    matches a literal `nx.`-prefixed holder — so the whole module silently produces no
    page. Resolve the alias and emit under the real namespace.
    """
    aliases = {}
    for line in lines:
        m = MODULE_ALIAS_RE.match(line)
        if m:
            aliases[m.group(1)] = m.group(2)
    if not aliases:
        return
    for i, line in enumerate(lines):
        m = SURFACE_METHOD_RE.match(line) or ALIAS_ASSIGN_RE.match(line)
        if not m:
            continue
        ns = aliases.get(m.group(1))
        if ns:
            yield ("%s.%s" % (ns, m.group(2)), m.group(3), i)


def collect_created_namespaces(lines):
    """The `nx.<NS>` namespaces a file creates via the `nx.X = nx.X or {}` idiom.

    Backs the coverage guard: a namespace that exists at runtime but produces no page is
    either undocumented or (rarely) data-only, and the two must be told apart explicitly.
    """
    out = set()
    for line in lines:
        m = re.match(r"^nx\.([A-Za-z0-9_]+)\s*=\s*nx\.\1\s+or\s+\{\}\s*$", line)
        if m:
            out.add(m.group(1))
    return out


def extract_api():
    if not os.path.isdir(PRELUDE_DIR):
        die("prelude dir not found: %s" % PRELUDE_DIR)
    # namespace -> list of (name, args, doc); insertion order preserved.
    namespaces = {}
    seen = set()
    created = set()  # every `nx.X = nx.X or {}` namespace, for the coverage guard
    files = sorted(f for f in os.listdir(PRELUDE_DIR) if f.endswith(".lua"))
    for fname in files:
        with open(os.path.join(PRELUDE_DIR, fname), encoding="utf-8") as f:
            lines = f.read().split("\n")
        # Gather every public declaration from the three real prelude shapes:
        # direct `function nx.NS.name` / `nx.NS.name = function`, namespace table
        # literals, and twin-surface factories. Sort by source line so a page's
        # entries stay in file order regardless of which collector found them.
        decls = []
        created |= collect_created_namespaces(lines)
        for i, line in enumerate(lines):
            m = DECL_RE.match(line)
            if m:
                name = m.group(1) or m.group(3)
                args = m.group(2) if m.group(1) else m.group(4)
                decls.append((i, name, args))
        for name, args, i in collect_table_literals(lines):
            decls.append((i, name, args))
        for name, args, i in collect_surface_factories(lines):
            decls.append((i, name, args))
        for name, args, i in collect_module_aliases(lines):
            decls.append((i, name, args))
        decls.sort(key=lambda d: d[0])

        for i, name, args in decls:
            if is_private(name) or name in seen:
                continue
            seen.add(name)
            parts = name[len("nx.") :].split(".")
            ns = "nx" if len(parts) == 1 else parts[0]
            doc = doc_above(lines, i)
            namespaces.setdefault(ns, []).append((name, args.strip(), doc, fname))

    if not namespaces:
        die("extracted zero nx.* declarations — extraction is broken")

    # COVERAGE GUARD. The check above only catches total failure; it cannot notice a single
    # module going missing, which is exactly what happened — `nx.plugins` (28 public
    # functions) and `nx.editorconfig` were absent for their whole existence because they
    # alias their namespace (`local M = nx.plugins`) and no collector resolved that. A
    # namespace that exists at runtime must either produce a page or be declared data-only,
    # so the next module written in an unrecognized style fails the build instead of
    # quietly vanishing.
    undocumented = sorted(
        ns for ns in created if not ns.startswith("_") and ns not in namespaces and ns not in NO_PUBLIC_API
    )
    if undocumented:
        die(
            "these nx.* namespaces exist in the prelude but produced no API page: %s\n"
            "       Either their functions are declared in a form no collector in this file\n"
            "       recognizes (add a collector), or they are genuinely data-only (add them\n"
            "       to NO_PUBLIC_API with a comment saying why)." % ", ".join(undocumented)
        )

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
