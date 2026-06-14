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
  '@tree-sitter-grammars/tree-sitter-zig': '1.1.2',
  '@tree-sitter-grammars/tree-sitter-lua': '0.4.1',
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
};

// The languages bundled into web/vendor/ for offline use. Kept small: rust (the repo's
// own language; verify-ui.mjs asserts it), lua (nxvim config), and the common
// json/javascript/typescript/python. Everything else is fetched on demand via
// `:TSInstall` and cached in OPFS.
export const BUNDLED = ['rust', 'lua', 'json', 'javascript', 'typescript', 'python'];

// The standard query kinds a `:TSInstall` fetches + caches (faithful to the native
// install). Only `highlights` is consumed by the current JS highlighter; the rest are
// cached for forward-compat (no browser consumer exists yet — see the plan's "out of
// scope"). `highlights` is assembled from the per-language `highlights` list above;
// the others come from the grammar package's own `queries/` dir, best-effort (a
// missing file is skipped, not an error).
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
export const FT = { c: 'cpp' };
