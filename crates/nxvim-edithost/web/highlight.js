// In-browser tree-sitter syntax highlighting for the nxvim web demo.
//
// The editor core (nxvim-core, compiled to WebAssembly) has no treesitter — that
// lives in nxvim-server, which this serverless build omits. So highlighting is a
// pure front-end layer, exactly the way the page already owns rendering: it parses
// the focused buffer's text with web-tree-sitter (the official WASM build of the
// tree-sitter runtime), runs the language's `highlights.scm`, and hands the renderer
// per-line capture spans to paint. Grammars + queries are vendored under ./vendor/
// (build.sh → npm run build:treesitter) and loaded lazily, one language at a time,
// the first time a file of that type is shown — so the initial page stays light.
//
// Everything tree-sitter reports is in UTF-16 code units, i.e. JS string indices, so
// a capture's [startIndex, endIndex) slices the source directly and a column maps to
// a `[...string]` position with no byte/char conversion (only tab expansion, which
// the renderer already does).

import { Parser, Language, Query } from './vendor/web-tree-sitter/web-tree-sitter.js';

// Resolve vendored assets relative to THIS module, not the page, so the demo still
// works if it's ever served from a sub-path.
const V = new URL('./vendor/', import.meta.url);

// File extension → vendored grammar. `.c`/`.h` route to the C++ grammar, which
// parses C fine for highlighting; the keys here must be languages the generator
// actually vendored (manifest.json gates this at runtime).
const EXT = {
  rs: 'rust',
  js: 'javascript', jsx: 'javascript', mjs: 'javascript', cjs: 'javascript',
  ts: 'typescript', mts: 'typescript', cts: 'typescript',
  tsx: 'tsx',
  json: 'json', jsonc: 'json',
  py: 'python', pyi: 'python', pyw: 'python',
  lua: 'lua',
  zig: 'zig',
  go: 'go',
  rb: 'ruby', rake: 'ruby', gemspec: 'ruby',
  php: 'php', phtml: 'php',
  cpp: 'cpp', cc: 'cpp', cxx: 'cpp', 'c++': 'cpp', hpp: 'cpp', hxx: 'cpp', hh: 'cpp',
  c: 'cpp', h: 'cpp', cu: 'cpp', cuh: 'cpp',
};

// nxvim filetype name → vendored grammar, for buffers whose language the editor
// resolved itself (an extension the core table knows, or an explicit `:set
// filetype=…` / `vim.treesitter.start`, both projected into the view as
// `window.filetype`). Only the names that *differ* from the grammar need an entry;
// everything else (rust, python, typescript, …) is assumed identical and falls
// through. `c` highlights with the C++ grammar, matching `EXT` above.
const FT = { c: 'cpp' };

// Capture group → CSS, in the One Dark family the demo's chrome already uses. Keys
// are matched most-specific first, then by trimming dotted suffixes (`function.call`
// → `function`), so only the distinctive sub-cases need their own entry.
const FG = {
  comment: '#5c6370',
  keyword: '#c678dd', conditional: '#c678dd', repeat: '#c678dd', exception: '#c678dd', include: '#c678dd',
  operator: '#56b6c2', 'keyword.operator': '#c678dd',
  string: '#98c379', character: '#98c379', escape: '#56b6c2', 'string.escape': '#56b6c2', 'string.special': '#56b6c2',
  number: '#d19a66', float: '#d19a66', boolean: '#d19a66',
  constant: '#d19a66', 'constant.builtin': '#d19a66',
  function: '#61afef', 'function.builtin': '#56b6c2', 'function.macro': '#56b6c2',
  constructor: '#e5c07b',
  type: '#e5c07b', 'type.builtin': '#e5c07b',
  property: '#e06c75', field: '#e06c75', 'variable.member': '#e06c75',
  variable: '#abb2bf', 'variable.parameter': '#abb2bf', parameter: '#abb2bf', 'variable.builtin': '#e06c75',
  label: '#61afef',
  punctuation: '#abb2bf', 'punctuation.bracket': '#abb2bf', 'punctuation.delimiter': '#abb2bf', 'punctuation.special': '#e06c75',
  attribute: '#e5c07b', annotation: '#e5c07b',
  namespace: '#e5c07b', module: '#e5c07b',
  tag: '#e06c75', 'tag.attribute': '#d19a66', 'tag.delimiter': '#abb2bf',
};

// Resolve a (possibly dotted) capture name to a CSS color, walking the fallback
// chain `a.b.c` → `a.b` → `a`. Returns null when nothing in the theme matches.
// Exported so the remote (server-styled) renderer can reuse this theme as its
// fallback for highlight spans the server sent without a resolved palette style.
export function colorFor(group) {
  let g = group;
  for (;;) {
    const c = FG[g];
    if (c) return c;
    const dot = g.lastIndexOf('.');
    if (dot < 0) return null;
    g = g.slice(0, dot);
  }
}

export function createHighlighter({ onReady } = {}) {
  let runtimeReady = false;
  let manifest = null; // Set<string> of vendored languages
  const langs = new Map(); // name → { parser, query } once loaded, or null if it failed
  const loading = new Set();
  // One-entry memo: re-highlighting the same buffer text (e.g. on a cursor move that
  // didn't edit) reuses the spans instead of re-parsing.
  let memo = { lang: null, text: null, spans: null };

  const notify = () => { if (onReady) onReady(); };

  (async () => {
    await Parser.init({ locateFile: (f) => new URL('web-tree-sitter/' + f, V).href });
    const res = await fetch(new URL('manifest.json', V));
    manifest = new Set((await res.json()).languages);
    runtimeReady = true;
    notify(); // repaint: files already on screen can highlight now
  })().catch((e) => console.error('[nxvim] tree-sitter runtime init failed:', e));

  async function load(name) {
    if (loading.has(name) || langs.has(name)) return;
    loading.add(name);
    try {
      const language = await Language.load(new URL('grammars/' + name + '.wasm', V).href);
      const src = await (await fetch(new URL('queries/' + name + '.scm', V))).text();
      const query = new Query(language, src);
      const parser = new Parser();
      parser.setLanguage(language);
      langs.set(name, { parser, query });
      notify(); // repaint with this language now available
    } catch (e) {
      // Fail loud in the console, silent (uncolored) in the UI — and don't retry.
      console.error('[nxvim] failed to load grammar', name, e);
      langs.set(name, null);
    } finally {
      loading.delete(name);
    }
  }

  // The vendored language for a file name, or null if unsupported / not yet ready.
  function langForName(name) {
    if (!name || !manifest) return null;
    const base = name.split(/[\\/]/).pop().toLowerCase();
    const dot = base.lastIndexOf('.');
    const ext = dot >= 0 ? base.slice(dot + 1) : '';
    const lang = EXT[ext];
    return lang && manifest.has(lang) ? lang : null;
  }

  // The vendored grammar for an editor filetype (`window.filetype` in the view),
  // or null if none is vendored / the runtime isn't ready. This is how `:set
  // filetype=…` and `vim.treesitter.start` highlight a buffer whose extension the
  // file-name path misses (or has none): the override-aware language the core
  // resolved wins over the extension.
  function langForFiletype(ft) {
    if (!ft || !manifest) return null;
    const lang = FT[ft] || ft;
    return manifest.has(lang) ? lang : null;
  }

  // Per-buffer-line capture spans for `text` in `lang`: an array indexed by 0-based
  // buffer line, each entry a list of `[startU16, endU16, group]`. Returns null when
  // the grammar isn't loaded yet — and kicks off the async load, after which onReady
  // fires a repaint. Multi-line captures (block comments, here-docs) are split at
  // line boundaries so each line carries only its own slice.
  function spansForBuffer(lang, text) {
    if (!lang) return null;
    if (!langs.has(lang)) { load(lang); return null; }
    const entry = langs.get(lang);
    if (!entry) return null; // load failed earlier
    if (memo.lang === lang && memo.text === text) return memo.spans;

    const tree = entry.parser.parse(text);
    const caps = entry.query.captures(tree.rootNode);
    const lines = text.split('\n');
    const lineStart = new Array(lines.length);
    for (let i = 0, off = 0; i < lines.length; i++) { lineStart[i] = off; off += lines[i].length + 1; }
    const spans = Array.from({ length: lines.length }, () => []);
    const lineAt = (idx) => {
      let lo = 0, hi = lines.length - 1;
      while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (lineStart[mid] <= idx) lo = mid; else hi = mid - 1; }
      return lo;
    };
    for (const c of caps) {
      const s = c.node.startIndex, e = c.node.endIndex;
      if (e <= s) continue;
      const endLn = lineAt(e - 1);
      for (let ln = lineAt(s); ln <= endLn; ln++) {
        const lb = lineStart[ln], le = lb + lines[ln].length;
        const a = Math.max(s, lb) - lb, b = Math.min(e, le) - lb;
        if (b > a) spans[ln].push([a, b, c.name]);
      }
    }
    tree.delete();
    memo = { lang, text, spans };
    return spans;
  }

  // Flatten one line's spans into a per-UTF-16-column color array (length `len`),
  // or null if the line has no spans. Wider captures are applied first so a narrower,
  // more specific capture overwrites them; equal-width ties go to the later capture
  // (query order). Columns with no color stay null.
  function colorsForLine(spans, len) {
    if (!spans || !spans.length) return null;
    const out = new Array(len).fill(null);
    const ordered = spans.map((s, i) => [s, i]).sort((x, y) => (y[0][1] - y[0][0]) - (x[0][1] - x[0][0]) || x[1] - y[1]);
    let any = false;
    for (const [[a, b, group]] of ordered) {
      const color = colorFor(group);
      if (!color) continue;
      for (let k = a; k < b && k < len; k++) { out[k] = color; any = true; }
    }
    return any ? out : null;
  }

  return {
    isReady: () => runtimeReady,
    langForName,
    langForFiletype,
    spansForBuffer,
    colorsForLine,
  };
}
