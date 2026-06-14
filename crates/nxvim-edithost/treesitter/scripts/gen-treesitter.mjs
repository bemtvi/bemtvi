// Vendor the tree-sitter assets the edit-host's highlighter needs into ./vendor/.
//
// For each supported language this copies (1) the prebuilt grammar `.wasm` that
// ships inside its pinned npm package and (2) a *sanitized* `highlights.scm`. The
// runtime (web-tree-sitter.js + .wasm) is copied once. Everything is regenerated
// from the pinned devDependencies, so ./vendor/ is gitignored — the parent crate's
// build.sh runs this generator and copies its output into web/vendor/.
//
// Why sanitize the queries: the upstream `highlights.scm` files occasionally use a
// predicate the browser query engine doesn't implement, or — when a grammar and
// its query drift across versions — reference a node type the compiled grammar
// lacks. Either makes the *whole* query fail to compile, which would silently kill
// highlighting for that language. So we load each grammar's real `.wasm` here, split
// its query into top-level patterns, and keep only the patterns that both compile
// and run against the grammar. With the matched versions pinned below nothing is
// dropped today; the pass exists so a future version bump degrades gracefully (a few
// patterns dropped) instead of cliff-edging to no highlighting — and prints what it
// dropped rather than hiding it.

import { Parser, Language, Query } from 'web-tree-sitter';
import { readFileSync, writeFileSync, mkdirSync, copyFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = dirname(dirname(fileURLToPath(import.meta.url))); // tooling dir
const NM = join(ROOT, 'node_modules');
const OUT = join(ROOT, 'vendor');

// Each language: the grammar package's `.wasm`, the query file(s) to concatenate
// (a base language's query first where a grammar is a superset — ts builds on js,
// cpp on c), and a representative source sample used to exercise the query so the
// sanitizer can drop any pattern that throws at *run* time, not just compile time.
const g = (p) => join(NM, p);
const LANGS = {
  rust: {
    wasm: g('tree-sitter-rust/tree-sitter-rust.wasm'),
    queries: [g('tree-sitter-rust/queries/highlights.scm')],
    sample: 'use std::collections::HashMap;\n/// doc\nconst MAX: u32 = 10;\npub fn f<T>(x: &mut T, y: u32) -> Option<u32> {\n    let s = "hi"; let c = \'a\';\n    if x == y { Some(y + 1) } else { None }\n}\nstruct S { a: i32 }\nenum E { A, B(u8) }\nmacro_rules! m { () => {} }',
  },
  javascript: {
    wasm: g('tree-sitter-javascript/tree-sitter-javascript.wasm'),
    queries: [g('tree-sitter-javascript/queries/highlights.scm'), g('tree-sitter-javascript/queries/highlights-jsx.scm')],
    sample: 'import {a} from "m";\n// c\nconst f = async (x) => { let s = `t${x}`; return /re/.test(s) ? 1 : null; }\nclass C extends B { #p = 1; get x() { return this.#p } }\nfunction* g() { yield* [1, 2] }',
  },
  typescript: {
    wasm: g('tree-sitter-typescript/tree-sitter-typescript.wasm'),
    queries: [g('tree-sitter-javascript/queries/highlights.scm'), g('tree-sitter-typescript/queries/highlights.scm')],
    sample: 'interface I<T> { a: number; b?: string }\ntype U = A | B;\nfunction f(x: number): Promise<void> { const s: string = `${x}`; return; }\nenum E { A, B }\nclass C implements I<number> { private a = 1 }',
  },
  tsx: {
    wasm: g('tree-sitter-typescript/tree-sitter-tsx.wasm'),
    queries: [g('tree-sitter-javascript/queries/highlights.scm'), g('tree-sitter-javascript/queries/highlights-jsx.scm'), g('tree-sitter-typescript/queries/highlights.scm')],
    sample: 'const A = (p: { n: number }) => <div className="x" onClick={() => p.n}>{p.n}<Sub/></div>;\ntype P = { n: number };',
  },
  json: {
    wasm: g('tree-sitter-json/tree-sitter-json.wasm'),
    queries: [g('tree-sitter-json/queries/highlights.scm')],
    sample: '{"a": 1, "b": [true, false, null, "s"], "c": {"d": 1.5e3}, "e": -2}',
  },
  python: {
    wasm: g('tree-sitter-python/tree-sitter-python.wasm'),
    queries: [g('tree-sitter-python/queries/highlights.scm')],
    sample: 'import os\nfrom x import y\n@deco\ndef f(a, *args, **kw):\n    """doc"""\n    s = f"hi{a}"\n    return [i for i in range(10) if i > 2]\nclass C(Base):\n    x: int = 1',
  },
  lua: {
    wasm: g('@tree-sitter-grammars/tree-sitter-lua/tree-sitter-lua.wasm'),
    queries: [g('@tree-sitter-grammars/tree-sitter-lua/queries/highlights.scm')],
    sample: 'local M = {}\nlocal function f(a, ...)\n  if a == nil then return "s" .. a end\n  for i = 1, 10 do print(i) end\nend\nfunction M.g() return true end\nreturn M',
  },
  zig: {
    wasm: g('@tree-sitter-grammars/tree-sitter-zig/tree-sitter-zig.wasm'),
    queries: [g('@tree-sitter-grammars/tree-sitter-zig/queries/highlights.scm')],
    sample: 'const std = @import("std");\npub fn main() !void {\n    var x: u32 = 42;\n    const s = "hi";\n    if (x > 0) { std.debug.print("{}", .{x}); }\n}\ntest "t" {}',
  },
  ruby: {
    wasm: g('tree-sitter-ruby/tree-sitter-ruby.wasm'),
    queries: [g('tree-sitter-ruby/queries/highlights.scm')],
    sample: 'require "set"\n# c\nclass C < B\n  def f(x)\n    @y = "s#{x}"\n    return :sym if x > 0\n    [1, 2].map { |i| i * 2 }\n  end\nend\nMAX = 10',
  },
  php: {
    wasm: g('tree-sitter-php/tree-sitter-php.wasm'),
    queries: [g('tree-sitter-php/queries/highlights.scm')],
    sample: '<?php\nnamespace App;\nclass C extends B {\n  private $x = 1;\n  public function f(int $a): string { return "s$a"; }\n}\n$y = [1, 2, 3];\necho "hi";',
  },
  cpp: {
    wasm: g('tree-sitter-cpp/tree-sitter-cpp.wasm'),
    queries: [g('tree-sitter-c/queries/highlights.scm'), g('tree-sitter-cpp/queries/highlights.scm')],
    sample: '#include <vector>\n#define M 1\ntemplate<typename T>\nclass C : public B {\n  int x_ = 0;\npublic:\n  T f(const T& a) { auto s = "hi"; return a; }\n};\nint main() { std::vector<int> v; return 0; }',
  },
  go: {
    wasm: g('tree-sitter-go/tree-sitter-go.wasm'),
    queries: [g('tree-sitter-go/queries/highlights.scm')],
    sample: 'package main\n\nimport "fmt"\n\ntype Point struct {\n\tX, Y float64\n}\n\nfunc (p Point) Add(q Point) Point {\n\treturn Point{p.X + q.X, p.Y + q.Y}\n}\n\nfunc main() {\n\tconst n = 3\n\ts := "hello"\n\tfor i := 0; i < n; i++ {\n\t\tfmt.Println(s, i)\n\t}\n}',
  },
};

// Split a tree-sitter query into top-level patterns. A pattern is one depth-0
// S-expression (or [..] alternation, or bare node) plus its trailing @captures,
// quantifiers, anchors, and (#predicate ..) groups.
function splitPatterns(src) {
  const pats = [];
  let i = 0;
  const n = src.length;
  const isWs = (c) => c === ' ' || c === '\t' || c === '\n' || c === '\r';
  const skipTrivia = () => {
    while (i < n) {
      if (isWs(src[i])) { i++; continue; }
      if (src[i] === ';') { while (i < n && src[i] !== '\n') i++; continue; }
      break;
    }
  };
  const consumeGroup = () => {
    let depth = 0;
    for (; i < n; i++) {
      const c = src[i];
      if (c === '"') { i++; while (i < n && src[i] !== '"') { if (src[i] === '\\') i++; i++; } continue; }
      if (c === ';') { while (i < n && src[i] !== '\n') i++; i--; continue; }
      if (c === '(' || c === '[') depth++;
      else if (c === ')' || c === ']') { depth--; if (depth === 0) { i++; return; } }
    }
  };
  const consumeBare = () => {
    if (src[i] === '"') { i++; while (i < n && src[i] !== '"') { if (src[i] === '\\') i++; i++; } i++; return; }
    while (i < n && !isWs(src[i]) && !'()[];'.includes(src[i])) i++;
  };
  const peekIsPredicate = () => {
    let j = i + 1;
    while (j < n && isWs(src[j])) j++;
    return src[j] === '#';
  };
  const consumeTrailers = () => {
    for (;;) {
      const save = i;
      skipTrivia();
      if (i >= n) return;
      const c = src[i];
      if (c === '@' || c === '.' || c === '?' || c === '*' || c === '+' || c === '!') { consumeBare(); continue; }
      if (c === '(' && peekIsPredicate()) { consumeGroup(); continue; }
      i = save;
      return;
    }
  };
  for (;;) {
    skipTrivia();
    if (i >= n) break;
    const start = i;
    if (src[i] === '(' || src[i] === '[') consumeGroup();
    else consumeBare();
    consumeTrailers();
    const text = src.slice(start, i).trim();
    if (text) pats.push(text);
  }
  return pats;
}

async function main() {
  await Parser.init();

  rmSync(OUT, { recursive: true, force: true });
  mkdirSync(join(OUT, 'web-tree-sitter'), { recursive: true });
  mkdirSync(join(OUT, 'grammars'), { recursive: true });
  mkdirSync(join(OUT, 'queries'), { recursive: true });

  // Runtime.
  copyFileSync(g('web-tree-sitter/web-tree-sitter.js'), join(OUT, 'web-tree-sitter', 'web-tree-sitter.js'));
  copyFileSync(g('web-tree-sitter/web-tree-sitter.wasm'), join(OUT, 'web-tree-sitter', 'web-tree-sitter.wasm'));

  const manifest = [];
  for (const [name, cfg] of Object.entries(LANGS)) {
    const lang = await Language.load(cfg.wasm);
    const parser = new Parser();
    parser.setLanguage(lang);
    const tree = parser.parse(cfg.sample);

    // `(#is? ...)` / `(#is-not? ...)` are the editor-only "is this a local?"
    // predicates; the browser engine has no locals table, so strip them outright.
    const raw = cfg.queries.map((p) => readFileSync(p, 'utf8')).join('\n').replace(/\(#is(-not)?\?[^()]*\)/g, '');
    const pats = splitPatterns(raw);
    const kept = [];
    let droppedCompile = 0;
    let droppedRun = 0;
    for (const pat of pats) {
      let q;
      try { q = new Query(lang, pat); } catch { droppedCompile++; continue; }
      try { q.captures(tree.rootNode); } catch { droppedRun++; continue; }
      kept.push(pat);
    }
    const queryText = kept.join('\n\n') + '\n';
    // Fail loud if the assembled query won't even compile — a broken vendored asset
    // should stop the build, not ship a language that silently never highlights.
    const caps = new Query(lang, queryText).captures(tree.rootNode).length;
    if (caps === 0) throw new Error(`${name}: sanitized query produced 0 captures on the sample — grammar/query mismatch?`);

    copyFileSync(cfg.wasm, join(OUT, 'grammars', `${name}.wasm`));
    writeFileSync(join(OUT, 'queries', `${name}.scm`), queryText);
    manifest.push(name);
    const dropped = droppedCompile + droppedRun;
    const note = dropped ? `  (dropped ${dropped}: ${droppedCompile} compile / ${droppedRun} run)` : '';
    console.log(`  ${name.padEnd(11)} ${kept.length}/${pats.length} patterns, ${caps} sample captures${note}`);
  }

  writeFileSync(join(OUT, 'manifest.json'), JSON.stringify({ languages: manifest }, null, 2) + '\n');
  console.log(`vendored ${manifest.length} languages + runtime into ./vendor/`);
}

main().catch((e) => {
  console.error('gen-treesitter failed:', e);
  process.exit(1);
});
