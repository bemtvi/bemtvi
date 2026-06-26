// The tree-sitter grammar registry — the single source of truth for which languages
// the edit-host can highlight, where each grammar's prebuilt `.wasm` + query files
// live in its pinned npm package, and how file names / editor filetypes map to a
// grammar. Shared by:
//   * the build-time vendor generator (treesitter/scripts/gen-treesitter.mjs), which
//     bundles the BUNDLED subset into web/vendor/ for offline use, and
//   * the runtime `:TSInstall` path (highlight.js), which fetches any other language
//     from jsDelivr at runtime and caches it in OPFS.
// Because both read this file, the offline set and the on-demand set can never use
// mismatched versions or paths.

// nvim-treesitter commit the INDENT queries are read from — pinned (matching
// `nxvim-ts`'s native `install.rs` `NVIM_TS_REF`) so the browser indents.scm is the
// same revision, in the same `@indent.begin`/`@indent.end` format the ported
// algorithm (web/ts-indent.js) expects. The grammar npm packages don't ship a usable
// `indents.scm` — only `highlights.scm` lives there — so indents come from
// nvim-treesitter's `runtime/queries/<lang>/indents.scm`, exactly like native.
export const NVIM_TS_REF = '4916d6592ede8c07973490d9322f187e07dfefac';

// The jsDelivr URL for `lang`'s nvim-treesitter `indents.scm` at the pinned ref, or
// null for a language not in the registry. `base` overrides the CDN host (a test
// mirror / the runtime's `__NXVIM_TS_BASE`). The nvim-treesitter query directory name
// is the registry's own canonical name (rust, python, c_sharp, …); a grammar that
// reuses another's parser (the FT map) still has its own query dir there.
export function indentSource(name, base) {
  if (!REGISTRY[name]) return null;
  const root = base || 'https://cdn.jsdelivr.net/gh';
  return `${root}/nvim-treesitter/nvim-treesitter@${NVIM_TS_REF}/runtime/queries/${name}/indents.scm`;
}

// The jsDelivr URL for `lang`'s nvim-treesitter `folds.scm` at the pinned ref, or null
// for a language not in the registry. Folds, like indents, come from nvim-treesitter
// (not the grammar npm package) so the browser folds match native exactly — native's
// `install.rs` copies the same nvim-treesitter `runtime/queries/<lang>/folds.scm`.
export function foldSource(name, base) {
  if (!REGISTRY[name]) return null;
  const root = base || 'https://cdn.jsdelivr.net/gh';
  return `${root}/nvim-treesitter/nvim-treesitter@${NVIM_TS_REF}/runtime/queries/${name}/folds.scm`;
}

// Pinned package versions — MUST stay in lockstep with treesitter/package.json (the
// build installs those exact versions; the runtime builds jsDelivr URLs from these).
export const VERSIONS = {
  'web-tree-sitter': '0.26.9',
  'tree-sitter-rust': '0.24.0',
  'tree-sitter-javascript': '0.25.0',
  'tree-sitter-typescript': '0.23.2',
  'tree-sitter-json': '0.24.8',
  'tree-sitter-python': '0.25.0',
  'tree-sitter-ruby': '0.23.1',
  'tree-sitter-php': '0.24.2',
  'tree-sitter-cpp': '0.23.4',
  'tree-sitter-c': '0.24.1',
  'tree-sitter-go': '0.25.0',
  'tree-sitter-bash': '0.25.1',
  'tree-sitter-css': '0.25.0',
  'tree-sitter-html': '0.23.2',
  'tree-sitter-java': '0.23.5',
  'tree-sitter-c-sharp': '0.23.5',
  '@tree-sitter-grammars/tree-sitter-zig': '1.1.2',
  '@tree-sitter-grammars/tree-sitter-lua': '0.4.1',
  '@tree-sitter-grammars/tree-sitter-toml': '0.7.0',
};

// Per language:
//   pkg         npm package holding the grammar `.wasm` and its `queries/` dir (the
//               source of the cached injections/indents/folds/locals query set).
//   wasm        the prebuilt parser `.wasm` subpath inside `pkg`.
//   highlights  ordered [pkg, file] list to concatenate into highlights.scm (a base
//               language first where a grammar is a superset — ts builds on js, cpp
//               on c). Defaults to `pkg`'s own queries/highlights.scm.
//   extensions  file extensions that select this grammar.
//   sample      a representative source, used to run-validate query patterns during
//               sanitize (drop patterns that throw against this grammar).
export const REGISTRY = {
  rust: {
    pkg: 'tree-sitter-rust',
    wasm: 'tree-sitter-rust.wasm',
    extensions: ['rs'],
    sample: 'use std::collections::HashMap;\n/// doc\nconst MAX: u32 = 10;\npub fn f<T>(x: &mut T, y: u32) -> Option<u32> {\n    let s = "hi"; let c = \'a\';\n    if x == y { Some(y + 1) } else { None }\n}\nstruct S { a: i32 }\nenum E { A, B(u8) }\nmacro_rules! m { () => {} }',
  },
  javascript: {
    pkg: 'tree-sitter-javascript',
    wasm: 'tree-sitter-javascript.wasm',
    highlights: [
      ['tree-sitter-javascript', 'queries/highlights.scm'],
      ['tree-sitter-javascript', 'queries/highlights-jsx.scm'],
    ],
    extensions: ['js', 'jsx', 'mjs', 'cjs'],
    sample: 'import {a} from "m";\n// c\nconst f = async (x) => { let s = `t${x}`; return /re/.test(s) ? 1 : null; }\nclass C extends B { #p = 1; get x() { return this.#p } }\nfunction* g() { yield* [1, 2] }',
  },
  typescript: {
    pkg: 'tree-sitter-typescript',
    wasm: 'tree-sitter-typescript.wasm',
    highlights: [
      ['tree-sitter-javascript', 'queries/highlights.scm'],
      ['tree-sitter-typescript', 'queries/highlights.scm'],
    ],
    extensions: ['ts', 'mts', 'cts'],
    sample: 'interface I<T> { a: number; b?: string }\ntype U = A | B;\nfunction f(x: number): Promise<void> { const s: string = `${x}`; return; }\nenum E { A, B }\nclass C implements I<number> { private a = 1 }',
  },
  tsx: {
    pkg: 'tree-sitter-typescript',
    wasm: 'tree-sitter-tsx.wasm',
    highlights: [
      ['tree-sitter-javascript', 'queries/highlights.scm'],
      ['tree-sitter-javascript', 'queries/highlights-jsx.scm'],
      ['tree-sitter-typescript', 'queries/highlights.scm'],
    ],
    extensions: ['tsx'],
    sample: 'const A = (p: { n: number }) => <div className="x" onClick={() => p.n}>{p.n}<Sub/></div>;\ntype P = { n: number };',
  },
  json: {
    pkg: 'tree-sitter-json',
    wasm: 'tree-sitter-json.wasm',
    extensions: ['json', 'jsonc'],
    sample: '{"a": 1, "b": [true, false, null, "s"], "c": {"d": 1.5e3}, "e": -2}',
  },
  python: {
    pkg: 'tree-sitter-python',
    wasm: 'tree-sitter-python.wasm',
    extensions: ['py', 'pyi', 'pyw'],
    sample: 'import os\nfrom x import y\n@deco\ndef f(a, *args, **kw):\n    """doc"""\n    s = f"hi{a}"\n    return [i for i in range(10) if i > 2]\nclass C(Base):\n    x: int = 1',
  },
  lua: {
    pkg: '@tree-sitter-grammars/tree-sitter-lua',
    wasm: 'tree-sitter-lua.wasm',
    extensions: ['lua'],
    sample: 'local M = {}\nlocal function f(a, ...)\n  if a == nil then return "s" .. a end\n  for i = 1, 10 do print(i) end\nend\nfunction M.g() return true end\nreturn M',
  },
  zig: {
    pkg: '@tree-sitter-grammars/tree-sitter-zig',
    wasm: 'tree-sitter-zig.wasm',
    extensions: ['zig'],
    sample: 'const std = @import("std");\npub fn main() !void {\n    var x: u32 = 42;\n    const s = "hi";\n    if (x > 0) { std.debug.print("{}", .{x}); }\n}\ntest "t" {}',
  },
  toml: {
    pkg: '@tree-sitter-grammars/tree-sitter-toml',
    wasm: 'tree-sitter-toml.wasm',
    extensions: ['toml'],
    sample: '# config\ntitle = "nxvim"\n[package]\nname = "demo"\nversion = "0.1.0"\nedition = 2024\nratio = 1.5\nenabled = true\nports = [8080, 8081]\n[deps]\nserde = { version = "1.0", features = ["derive"] }\n[[bin]]\nname = "main"\npublished = 2026-06-15',
  },
  ruby: {
    pkg: 'tree-sitter-ruby',
    wasm: 'tree-sitter-ruby.wasm',
    extensions: ['rb', 'rake', 'gemspec'],
    sample: 'require "set"\n# c\nclass C < B\n  def f(x)\n    @y = "s#{x}"\n    return :sym if x > 0\n    [1, 2].map { |i| i * 2 }\n  end\nend\nMAX = 10',
  },
  php: {
    pkg: 'tree-sitter-php',
    wasm: 'tree-sitter-php.wasm',
    extensions: ['php', 'phtml'],
    sample: '<?php\nnamespace App;\nclass C extends B {\n  private $x = 1;\n  public function f(int $a): string { return "s$a"; }\n}\n$y = [1, 2, 3];\necho "hi";',
  },
  cpp: {
    pkg: 'tree-sitter-cpp',
    wasm: 'tree-sitter-cpp.wasm',
    highlights: [
      ['tree-sitter-c', 'queries/highlights.scm'],
      ['tree-sitter-cpp', 'queries/highlights.scm'],
    ],
    // The C++ grammar parses C fine, so C files highlight with it (see FT/EXT below).
    extensions: ['cpp', 'cc', 'cxx', 'c++', 'hpp', 'hxx', 'hh', 'c', 'h', 'cu', 'cuh'],
    sample: '#include <vector>\n#define M 1\ntemplate<typename T>\nclass C : public B {\n  int x_ = 0;\npublic:\n  T f(const T& a) { auto s = "hi"; return a; }\n};\nint main() { std::vector<int> v; return 0; }',
  },
  go: {
    pkg: 'tree-sitter-go',
    wasm: 'tree-sitter-go.wasm',
    extensions: ['go'],
    sample: 'package main\n\nimport "fmt"\n\ntype Point struct {\n\tX, Y float64\n}\n\nfunc (p Point) Add(q Point) Point {\n\treturn Point{p.X + q.X, p.Y + q.Y}\n}\n\nfunc main() {\n\tconst n = 3\n\ts := "hello"\n\tfor i := 0; i < n; i++ {\n\t\tfmt.Println(s, i)\n\t}\n}',
  },
  bash: {
    pkg: 'tree-sitter-bash',
    wasm: 'tree-sitter-bash.wasm',
    extensions: ['sh', 'bash'],
    sample: '#!/usr/bin/env bash\nset -euo pipefail\n# comment\nNAME="world"\ngreet() {\n  local who=$1\n  echo "hello ${NAME} $who"\n}\nfor i in 1 2 3; do greet "$i"; done\nif [[ -f "$HOME/.bashrc" ]]; then source "$HOME/.bashrc"; fi',
  },
  css: {
    pkg: 'tree-sitter-css',
    wasm: 'tree-sitter-css.wasm',
    extensions: ['css'],
    sample: '/* comment */\n:root { --accent: #3366ff; }\n.card, #main > .row:hover {\n  color: var(--accent);\n  margin: 0 auto;\n  font-size: 1.5rem;\n}\n@media (max-width: 600px) { .card { display: none; } }',
  },
  html: {
    pkg: 'tree-sitter-html',
    wasm: 'tree-sitter-html.wasm',
    extensions: ['html', 'htm'],
    sample: '<!DOCTYPE html>\n<html lang="en">\n<head><meta charset="utf-8"><title>T</title></head>\n<body>\n  <!-- comment -->\n  <div class="x" id="y"><p>Hello <a href="#">link</a></p></div>\n</body>\n</html>',
  },
  java: {
    pkg: 'tree-sitter-java',
    wasm: 'tree-sitter-java.wasm',
    extensions: ['java'],
    sample: 'package com.example;\nimport java.util.List;\n/** doc */\npublic class C<T> extends B implements I {\n  private final int x = 1;\n  public static void main(String[] args) {\n    String s = "hi";\n    for (int i = 0; i < 10; i++) { System.out.println(s + i); }\n  }\n}',
  },
  // npm package is `tree-sitter-c-sharp` (hyphen); its prebuilt parser is named with an
  // underscore (`tree-sitter-c_sharp.wasm`). Reached via `:TSInstall c#` / `csharp` / `cs`
  // through ALIASES below; `.cs`/`.csx` files select it directly.
  c_sharp: {
    pkg: 'tree-sitter-c-sharp',
    wasm: 'tree-sitter-c_sharp.wasm',
    extensions: ['cs', 'csx'],
    sample: 'using System;\nnamespace App {\n  public class C : B {\n    private int _x = 1;\n    public string Name { get; set; }\n    public async Task<int> F(int a) {\n      var s = $"hi{a}";\n      return await Task.FromResult(a + _x);\n    }\n  }\n  enum E { A, B }\n}',
  },
  // JSX is parsed by the JavaScript grammar (no separate parser, unlike `tsx`/typescript),
  // so this reuses tree-sitter-javascript + its JSX highlights. `.jsx` files keep mapping to
  // the bundled `javascript` (see its `extensions`) so they highlight offline; this entry
  // exists so `:TSInstall jsx` resolves to the same JSX-aware query set.
  jsx: {
    pkg: 'tree-sitter-javascript',
    wasm: 'tree-sitter-javascript.wasm',
    highlights: [
      ['tree-sitter-javascript', 'queries/highlights.scm'],
      ['tree-sitter-javascript', 'queries/highlights-jsx.scm'],
    ],
    sample: 'import React from "react";\nconst App = ({name}) => {\n  const [n, setN] = React.useState(0);\n  return <div className="x" onClick={() => setN(n + 1)}>Hello {name}: {n}<Sub/></div>;\n};\nfunction Sub() { return <span>!</span>; }',
  },
};

// The languages bundled into web/vendor/ for offline use. Kept small: rust (the repo's
// own language; verify-ui.mjs asserts it), lua (nxvim config), and the common
// json/javascript/typescript/python. Everything else is fetched on demand via
// `:TSInstall` and cached in OPFS.
export const BUNDLED = ['rust', 'lua', 'json', 'javascript', 'typescript', 'python'];

// The standard query kinds a `:TSInstall` fetches + caches (faithful to the native
// install). `highlights` feeds the UI highlighter and `indents` the worker's tree-sitter
// indenter (web/ts-indent.js); injections/folds/locals are cached for forward-compat (no
// browser consumer yet). `highlights` is assembled from the per-language `highlights` list
// above; `indents` comes from nvim-treesitter (see `indentSource`, since the grammar
// packages don't ship a usable one); the others come from the grammar package's own
// `queries/` dir, all best-effort (a missing file is skipped, not an error).
export const QUERY_KINDS = ['highlights', 'injections', 'indents', 'folds', 'locals'];

// Resolve the [pkg, file] query sources for a language's highlights.scm. Defaults to
// the grammar package's own queries/highlights.scm when no override list is given.
export function highlightSources(name) {
  const cfg = REGISTRY[name];
  if (!cfg) return [];
  return cfg.highlights || [[cfg.pkg, 'queries/highlights.scm']];
}

// The pinned version for a package (throws loud on an unpinned package rather than
// fetching a floating "latest" — versions are pinned exactly, like the Cargo deps).
export function versionOf(pkg) {
  const v = VERSIONS[pkg];
  if (!v) throw new Error(`grammars.js: no pinned version for package '${pkg}'`);
  return v;
}

// extension → grammar name, derived from each entry's `extensions`.
export const EXT = (() => {
  const m = {};
  for (const [name, cfg] of Object.entries(REGISTRY)) {
    for (const ext of cfg.extensions || []) m[ext] = name;
  }
  return m;
})();

// editor-filetype → grammar name, for buffers whose language the core resolved itself
// (an explicit `:set filetype=…` or an extension the core table knows). Only names
// that *differ* from the grammar need an entry; `c` highlights with the C++ grammar.
export const FT = { c: 'cpp', sh: 'bash', cs: 'c_sharp', csharp: 'c_sharp' };

// Friendly `:TSInstall <arg>` aliases → canonical registry name. The command argument is
// resolved through this before the REGISTRY lookup, so `:TSInstall c#` / `csharp` / `cs`
// all install `c_sharp`, and `sh` / `shell` install `bash`. File-name and filetype
// resolution don't use this (they go through EXT / FT); it's only for the install command.
export const ALIASES = {
  'c#': 'c_sharp',
  csharp: 'c_sharp',
  cs: 'c_sharp',
  sh: 'bash',
  shell: 'bash',
  htm: 'html',
};

// Canonicalize a `:TSInstall` language argument (lower-cased, alias-resolved). Unknown
// names pass through unchanged so the caller can report `unknown language '<name>'`.
export function resolveName(name) {
  const k = String(name || '').toLowerCase();
  return ALIASES[k] || k;
}
