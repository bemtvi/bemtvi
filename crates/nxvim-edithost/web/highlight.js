// In-browser tree-sitter syntax highlighting for the nxvim edit-host.
//
// The editor core (nxvim-core, compiled to WebAssembly) has no treesitter — that
// lives in nxvim-server, which this serverless build omits. So highlighting is a
// pure front-end layer, exactly the way the page already owns rendering: it parses
// the focused buffer's text with web-tree-sitter (the official WASM build of the
// tree-sitter runtime), runs the language's `highlights.scm`, and hands the renderer
// per-line capture spans to paint.
//
// Grammars come from two places, resolved at runtime (see the `grammars.js` registry):
//   * a small OFFLINE bundle vendored under ./vendor/ (build.sh → gen-treesitter), and
//   * anything else, fetched on demand by `:TSInstall <lang>` from a CDN, sanitized,
//     and cached in OPFS so it survives reloads.
// The runtime knows the *names* of every registry language (so it maps file → grammar),
// but only highlights one whose grammar is actually available (bundled or installed) —
// opening a file of an un-installed language renders plain until you `:TSInstall` it.
//
// Everything tree-sitter reports is in UTF-16 code units, i.e. JS string indices, so
// a capture's [startIndex, endIndex) slices the source directly and a column maps to
// a `[...string]` position with no byte/char conversion (only tab expansion, which
// the renderer already does).

import { Parser, Language, Query } from './vendor/web-tree-sitter/web-tree-sitter.js';
import { EXT, FT, REGISTRY, QUERY_KINDS, highlightSources, versionOf } from './grammars.js';
import { sanitize } from './ts-sanitize.js';

// Resolve vendored assets relative to THIS module, not the page, so the demo still
// works if it's ever served from a sub-path.
const V = new URL('./vendor/', import.meta.url);

// Where `:TSInstall` fetches a non-bundled grammar's prebuilt `.wasm` + queries. The
// pinned npm packages are served verbatim by jsDelivr (permissive CORS, so the cross-
// origin-isolated page's COEP `require-corp` accepts the cors-mode fetch). Overridable
// via a global so a hermetic test can point it at a local mirror.
const CDN_BASE = (typeof globalThis !== 'undefined' && globalThis.__NXVIM_TS_BASE) || 'https://cdn.jsdelivr.net/npm';

// OPFS path (under the same `.nxvim` dir the worker keeps shada in) where installed
// grammars are cached: `/.nxvim/treesitter/<lang>/{parser.wasm,highlights.scm,…}` plus
// a `manifest.json` listing what's installed.
const TS_DIR = ['.nxvim', 'treesitter'];

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

// --- OPFS helpers (UI thread) -------------------------------------------------------
// The highlighter runs on the page (window) thread, where OPFS is reached via the async
// File System Access API (getFile/createWritable) — the synchronous access handles are
// worker-only, but the grammar cache is small and off the hot path, so async is fine.

async function opfsDir(parts, create) {
  let dir = await navigator.storage.getDirectory();
  for (const p of parts) dir = await dir.getDirectoryHandle(p, { create });
  return dir;
}
async function opfsReadBytes(parts) {
  const dir = await opfsDir(parts.slice(0, -1), false);
  const fh = await dir.getFileHandle(parts[parts.length - 1], { create: false });
  return new Uint8Array(await (await fh.getFile()).arrayBuffer());
}
async function opfsReadText(parts) {
  return new TextDecoder().decode(await opfsReadBytes(parts));
}
async function opfsWriteBytes(parts, bytes) {
  const dir = await opfsDir(parts.slice(0, -1), true);
  const fh = await dir.getFileHandle(parts[parts.length - 1], { create: true });
  const w = await fh.createWritable();
  await w.write(bytes);
  await w.close();
}

async function fetchText(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${r.status} fetching ${url}`);
  return r.text();
}
async function fetchBytes(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${r.status} fetching ${url}`);
  return new Uint8Array(await r.arrayBuffer());
}

export function createHighlighter({ onReady } = {}) {
  let runtimeReady = false;
  const bundled = new Set(); // languages vendored offline in ./vendor/
  const installed = new Set(); // languages cached in OPFS by `:TSInstall`
  const langs = new Map(); // name → { parser, query } once loaded, or null if it failed
  const loading = new Set();
  // One-entry memo: re-highlighting the same buffer text (e.g. on a cursor move that
  // didn't edit) reuses the spans instead of re-parsing.
  let memo = { lang: null, text: null, spans: null };

  const notify = () => { if (onReady) onReady(); };
  const isAvail = (name) => bundled.has(name) || installed.has(name);

  // A grammar is available offline (bundled) or cached (OPFS); both gate highlighting.
  let readyResolve;
  const ready = new Promise((r) => { readyResolve = r; });

  (async () => {
    await Parser.init({ locateFile: (f) => new URL('web-tree-sitter/' + f, V).href });
    // The offline bundle's manifest (always present — gen-treesitter writes it).
    try {
      const res = await fetch(new URL('manifest.json', V));
      if (res.ok) for (const l of (await res.json()).languages) bundled.add(l);
    } catch (e) {
      console.error('[nxvim] vendor manifest load failed:', e);
    }
    // The OPFS install cache's manifest (absent until the first `:TSInstall`).
    try {
      for (const l of JSON.parse(await opfsReadText([...TS_DIR, 'manifest.json']))) installed.add(l);
    } catch { /* nothing installed yet */ }
    runtimeReady = true;
    readyResolve();
    notify(); // repaint: files already on screen can highlight now
  })().catch((e) => console.error('[nxvim] tree-sitter runtime init failed:', e));

  // Load a grammar into memory from its cache — OPFS first (an installed grammar wins,
  // so a `:TSInstall` of a name that's also bundled uses the freshly fetched copy), else
  // the offline bundle. Both stores hold an already-sanitized highlights.scm, so this
  // just compiles it. Fails loud in the console, silent (uncolored) in the UI.
  async function load(name) {
    if (loading.has(name) || langs.has(name)) return;
    loading.add(name);
    try {
      let language, src;
      if (installed.has(name)) {
        language = await Language.load(await opfsReadBytes([...TS_DIR, name, 'parser.wasm']));
        src = await opfsReadText([...TS_DIR, name, 'highlights.scm']);
      } else {
        language = await Language.load(new URL('grammars/' + name + '.wasm', V).href);
        src = await fetchText(new URL('queries/' + name + '.scm', V).href);
      }
      const query = new Query(language, src);
      const parser = new Parser();
      parser.setLanguage(language);
      langs.set(name, { parser, query });
      notify(); // repaint with this language now available
    } catch (e) {
      console.error('[nxvim] failed to load grammar', name, e);
      langs.set(name, null);
    } finally {
      loading.delete(name);
    }
  }

  // `:TSInstall <name>` — make a grammar available. If it's already bundled/installed,
  // (re)register it (no network). Otherwise fetch the prebuilt `.wasm` + the standard
  // query set from the CDN, sanitize highlights against the grammar, cache everything in
  // OPFS (so it survives reload), and register it. Returns `{ ok, msg }` for the status
  // line. Only highlights.scm is consumed today; indents/injections/folds/locals are
  // cached for forward-compat (no browser consumer yet).
  async function install(name) {
    await ready;
    const cfg = REGISTRY[name];
    if (!cfg) return { ok: false, msg: `unknown language '${name}'` };
    // Already available — re-register from cache (also the `:TSUpdate` of a bundled lang).
    if (isAvail(name)) {
      langs.delete(name);
      await load(name);
      return langs.get(name)
        ? { ok: true, msg: installed.has(name) ? 'already installed' : 'bundled' }
        : { ok: false, msg: 'cached grammar failed to load' };
    }
    try {
      const ver = versionOf(cfg.pkg);
      const wasmBytes = await fetchBytes(`${CDN_BASE}/${cfg.pkg}@${ver}/${cfg.wasm}`);
      const language = await Language.load(wasmBytes);

      // highlights.scm — concatenate the registry's source list (ts builds on js, …),
      // then sanitize against the grammar (drop predicates / patterns it can't run).
      const rawHl = (
        await Promise.all(highlightSources(name).map(([pkg, file]) =>
          fetchText(`${CDN_BASE}/${pkg}@${versionOf(pkg)}/${file}`)))
      ).join('\n');
      const parser = new Parser();
      parser.setLanguage(language);
      const tree = parser.parse(cfg.sample);
      const res = sanitize(rawHl, Query, language, tree.rootNode);
      const query = new Query(language, res.text);
      const caps = query.captures(tree.rootNode).length;
      tree.delete();
      if (caps === 0) return { ok: false, msg: 'grammar/query mismatch (0 captures)' };

      // The rest of the standard query set — cached for forward-compat, best-effort
      // (a grammar that doesn't ship one is skipped, not an error).
      const extras = {};
      for (const kind of QUERY_KINDS) {
        if (kind === 'highlights') continue;
        try { extras[kind] = await fetchText(`${CDN_BASE}/${cfg.pkg}@${ver}/queries/${kind}.scm`); }
        catch { /* not shipped by this grammar */ }
      }

      // Persist to OPFS, then register in memory + the installed manifest.
      const enc = (s) => new TextEncoder().encode(s);
      await opfsWriteBytes([...TS_DIR, name, 'parser.wasm'], wasmBytes);
      await opfsWriteBytes([...TS_DIR, name, 'highlights.scm'], enc(res.text));
      for (const [kind, text] of Object.entries(extras)) {
        await opfsWriteBytes([...TS_DIR, name, kind + '.scm'], enc(text));
      }
      installed.add(name);
      await opfsWriteBytes([...TS_DIR, 'manifest.json'], enc(JSON.stringify([...installed])));

      langs.set(name, { parser, query });
      memo = { lang: null, text: null, spans: null }; // a now-installed buffer must re-highlight
      notify();
      const kinds = ['highlights', ...Object.keys(extras)].join('+');
      return { ok: true, msg: `${cfg.pkg}@${ver}, queries: ${kinds}` };
    } catch (e) {
      return { ok: false, msg: String(e && e.message ? e.message : e) };
    }
  }

  // The grammar for a file name, or null if unsupported / not yet available. A known
  // extension whose grammar isn't installed returns null (no auto-fetch) — `:TSInstall`
  // is the explicit gate for pulling a grammar over the network.
  function langForName(name) {
    if (!name || !runtimeReady) return null;
    const base = name.split(/[\\/]/).pop().toLowerCase();
    const dot = base.lastIndexOf('.');
    const ext = dot >= 0 ? base.slice(dot + 1) : '';
    const lang = EXT[ext];
    return lang && isAvail(lang) ? lang : null;
  }

  // The grammar for an editor filetype (`window.filetype` in the view), or null if none
  // is available. This is how `:set filetype=…` highlights a buffer whose extension the
  // file-name path misses (or has none): the override-aware language the core resolved.
  function langForFiletype(ft) {
    if (!ft || !runtimeReady) return null;
    const lang = FT[ft] || ft;
    return isAvail(lang) ? lang : null;
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
    install,
  };
}
