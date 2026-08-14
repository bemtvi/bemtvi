// In-browser tree-sitter syntax highlighting for the bemtvi edit-host.
//
// The wasm edit-host has no in-process tree-sitter parser — the native engine
// (bemtvi-ts) is gated off this build (only its synchronous indenter is reimplemented
// in JS, web/ts-indent.js). So highlighting is a pure front-end layer, exactly the way
// the page already owns rendering: it parses
// the focused buffer's text with web-tree-sitter (the official WASM build of the
// tree-sitter runtime), runs the language's `highlights.scm`, and hands the renderer
// per-line capture spans to paint.
//
// Grammars come from two places, resolved at runtime (see the `grammars.js` registry):
//   * a small OFFLINE bundle vendored under ./vendor/ (build.sh → gen-treesitter), and
//   * anything else, fetched on demand by `:TSInstall <lang>` from a CDN, sanitized,
//     and cached in OPFS so it survives reloads.
// Embedded content is highlighted too: a grammar that vendors an `injections.scm` has it
// run here, so a markdown buffer's prose reaches the markdown_inline grammar and the code
// in its ```` ```lang ```` fences reaches that language's (see `paintLang`).
// The runtime knows the *names* of every registry language (so it maps file → grammar),
// but only highlights one whose grammar is actually available (bundled or installed) —
// opening a file of an un-installed language renders plain until you `:TSInstall` it.
//
// Everything tree-sitter reports is in UTF-16 code units, i.e. JS string indices, so
// a capture's [startIndex, endIndex) slices the source directly and a column maps to
// a `[...string]` position with no byte/char conversion (only tab expansion, which
// the renderer already does).

import { Parser, Language, Query } from './vendor/web-tree-sitter/web-tree-sitter.js';
import { EXT, FT, REGISTRY, QUERY_KINDS, highlightSources, fetchQueryMerged, versionOf, resolveName } from './grammars.js';
import { sanitize } from './ts-sanitize.js';

// Resolve vendored assets relative to THIS module, not the page, so the demo still
// works if it's ever served from a sub-path.
const V = new URL('./vendor/', import.meta.url);

// Where `:TSInstall` fetches a non-bundled grammar's prebuilt `.wasm` + queries. The
// pinned npm packages are served verbatim by jsDelivr (permissive CORS, so the cross-
// origin-isolated page's COEP `require-corp` accepts the cors-mode fetch). Overridable
// via a global so a hermetic test can point it at a local mirror.
const CDN_BASE = (typeof globalThis !== 'undefined' && globalThis.__BEMTVI_TS_BASE) || 'https://cdn.jsdelivr.net/npm';

// Where the nvim-treesitter INDENT queries are fetched from (jsDelivr's GitHub mirror).
// The grammar npm packages don't ship a usable `indents.scm`, so indents come from
// nvim-treesitter at the pinned ref — same source as the bundle generator and native.
// Overridable for a hermetic test.
const GH_BASE = (typeof globalThis !== 'undefined' && globalThis.__BEMTVI_TS_GH_BASE) || 'https://cdn.jsdelivr.net/gh';

// How deep an injection chain is followed: a buffer's own language is depth 0, the
// content it injects depth 1, and so on. Markdown needs 2 (block → inline → the html /
// latex that inline injects); the cap is what stops a grammar pair that injects each
// other from recursing forever.
const MAX_INJECTION_DEPTH = 3;

// Injected content is highlighted only around the VISIBLE lines, and this is the band the
// visible range is rounded out to. Why bound it at all: an injection costs one small parse
// per region, and a region is a paragraph or a fence — a 6600-line markdown file has ~3000
// of them, ~200ms of parsing, which would land on every repaint that changed the text. The
// native engine runs its queries over the visible byte range for the same reason. Why round
// to a band instead of using the viewport exactly: the painted range is part of the memo
// key, so an exact range would invalidate the memo — and re-parse the whole buffer — on
// every single line scrolled. Quantized, that happens only when the viewport crosses a band
// edge, and the band's own injections are ~1ms.
const INJECTION_BAND = 200;

// Capture groups whose highlight is a full-line BACKGROUND, not a foreground — markdown's
// fenced and indented code blocks (`(fenced_code_block) @markup.raw.block`). The native
// engine keeps these as spans and additionally reports the lines they touch, so the server
// can paint them as its separate `line_bg` layer *under* the text (`LINE_BACKGROUND_GROUPS`
// in bemtvi-ts). This build has no such layer — a span here is only ever a foreground
// color — so painting one would recolor the whole code block instead of tinting behind it,
// hiding the injected syntax. Drop them at the source and let the injected language paint.
const LINE_BACKGROUND_CAPTURES = new Set(['markup.raw.block']);

// OPFS path (under the same `.bemtvi` dir the worker keeps shada in) where installed
// grammars are cached: `/.bemtvi/treesitter/<lang>/{parser.wasm,highlights.scm,…}` plus
// a `manifest.json` listing what's installed.
const TS_DIR = ['.bemtvi', 'treesitter'];

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
  'keyword.directive': '#c678dd',
  // Markup captures — a markdown buffer (the block grammar plus everything the
  // markdown_inline injection adds), mirroring the built-in colorscheme's `@markup.*`
  // (crates/bemtvi-server/runtime/colors/bemtvi.lua) in the One Dark family above.
  // Heading levels are spelled out rather than left to the `markup.heading` fallback so
  // they step blue → cyan → green the way the real theme does. Emphasis is a *color*
  // here: these spans carry a foreground and nothing else, so the bold/italic the theme
  // uses for `@markup.strong` / `@markup.italic` can't render — they get distinct hues
  // instead. `markup.raw.block` is deliberately ABSENT (see LINE_BACKGROUND_CAPTURES).
  'markup.heading': '#61afef',
  'markup.heading.1': '#61afef', 'markup.heading.2': '#61afef',
  'markup.heading.3': '#56b6c2', 'markup.heading.4': '#56b6c2',
  'markup.heading.5': '#98c379', 'markup.heading.6': '#98c379',
  'markup.strong': '#e5c07b', 'markup.italic': '#c678dd', 'markup.strikethrough': '#5c6370',
  'markup.raw': '#98c379',
  'markup.link': '#56b6c2', 'markup.link.label': '#61afef', 'markup.link.url': '#56b6c2',
  'markup.list': '#d19a66', 'markup.list.checked': '#98c379', 'markup.list.unchecked': '#5c6370',
  'markup.quote': '#5c6370',
};

// The active colorscheme's syntax palette (`capture-name → CSS color`), bridged from
// the core's highlight registry on the wasm build (server `theme` map → `index.html`
// → here). Consulted by `colorFor` BEFORE the static `FG` fallback, so loading a
// colorscheme (catppuccin, …) recolors code to match. `null` until a frame carries a
// theme; an unthemed capture falls through to `FG` (the built-in One Dark family).
let runtimeTheme = null;

// Install the colorscheme syntax palette (or `null` to clear). The keys mirror `FG`'s
// (dotted capture names); each value is a CSS color string. Callers repaint after.
export function setHlTheme(map) {
  runtimeTheme = map && Object.keys(map).length ? map : null;
}

// Resolve a (possibly dotted) capture name to a CSS color, walking the fallback
// chain `a.b.c` → `a.b` → `a` — over the active colorscheme palette first, then the
// static `FG` table. Returns null when nothing matches.
// Exported so the remote (server-styled) renderer can reuse this theme as its
// fallback for highlight spans the server sent without a resolved palette style.
export function colorFor(group) {
  let g = group;
  for (;;) {
    const c = (runtimeTheme && runtimeTheme[g]) || FG[g];
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
  const bundledIndents = new Set(); // bundled languages that ALSO vendor an indents.scm
  const bundledInjections = new Set(); // bundled languages that ALSO vendor an injections.scm
  const installed = new Set(); // languages cached in OPFS by `:TSInstall`
  const langs = new Map(); // name → { parser, query, inj } once loaded, or null if it failed
  const loading = new Set();
  // One-entry memo: re-highlighting the same buffer text (e.g. on a cursor move that
  // didn't edit) reuses the spans instead of re-parsing. Keyed by the painted injection
  // band too (`from`/`to`), since spans outside it lack their injected content.
  let memo = { lang: null, text: null, from: 0, to: 0, spans: null };

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
      console.error('[bemtvi] vendor manifest load failed:', e);
    }
    // Which bundled grammars also vendor an indents.scm (gen-treesitter only writes one
    // when nvim-treesitter shipped indents for that language). Bundled grammars NOT listed
    // here highlight offline but have no indents until `:TSInstall` fetches them — see
    // `hasIndents` / `install`.
    try {
      const res = await fetch(new URL('indents.json', V));
      if (res.ok) for (const l of await res.json()) bundledIndents.add(l);
    } catch { /* no bundled indents manifest */ }
    // Which bundled grammars vendor an injections.scm — the query that sends embedded
    // content (a markdown `(inline)` node, a ```lang fence) to its own grammar. Listed so
    // `load` fetches one only where it exists instead of probing for a 404.
    try {
      const res = await fetch(new URL('injections.json', V));
      if (res.ok) for (const l of await res.json()) bundledInjections.add(l);
    } catch { /* no bundled injections manifest */ }
    // The OPFS install cache's manifest (absent until the first `:TSInstall`).
    try {
      for (const l of JSON.parse(await opfsReadText([...TS_DIR, 'manifest.json']))) installed.add(l);
    } catch { /* nothing installed yet */ }
    runtimeReady = true;
    readyResolve();
    notify(); // repaint: files already on screen can highlight now
  })().catch((e) => console.error('[bemtvi] tree-sitter runtime init failed:', e));

  // Load a grammar into memory from its cache — OPFS first (an installed grammar wins,
  // so a `:TSInstall` of a name that's also bundled uses the freshly fetched copy), else
  // the offline bundle. Both stores hold an already-sanitized highlights.scm, so this
  // just compiles it, plus the injections.scm where the grammar has one. Fails loud in
  // the console, silent (uncolored) in the UI.
  async function load(name) {
    if (loading.has(name) || langs.has(name)) return;
    loading.add(name);
    try {
      let language, src, injSrc;
      if (installed.has(name)) {
        language = await Language.load(await opfsReadBytes([...TS_DIR, name, 'parser.wasm']));
        src = await opfsReadText([...TS_DIR, name, 'highlights.scm']);
        // `:TSInstall` caches the whole standard query set, but an older install (or a
        // grammar that ships none) may have no injections.scm — absent is not an error.
        try { injSrc = await opfsReadText([...TS_DIR, name, 'injections.scm']); } catch { injSrc = null; }
      } else {
        language = await Language.load(new URL('grammars/' + name + '.wasm', V).href);
        src = await fetchText(new URL('queries/' + name + '.scm', V).href);
        injSrc = bundledInjections.has(name)
          ? await fetchText(new URL('injections/' + name + '.scm', V).href)
          : null;
      }
      const query = new Query(language, src);
      // The injections query is optional: a grammar without one simply highlights its own
      // text end to end. It's already sanitized against this grammar (build time for the
      // bundle, install time for OPFS), so a compile failure here is a broken asset — loud
      // in the console, and only injections are lost.
      let inj = null;
      if (injSrc && injSrc.trim()) {
        try { inj = new Query(language, injSrc); }
        catch (e) { console.error('[bemtvi] injections query failed to compile for', name, e); }
      }
      const parser = new Parser();
      parser.setLanguage(language);
      langs.set(name, { parser, query, inj });
      // A newly available grammar changes what an ALREADY-highlighted buffer's spans should
      // be, not just an unhighlighted one: a markdown buffer paints block-only while the
      // markdown_inline it injects is still loading, and its fenced code stays plain until
      // that fence's language lands. Each arrives on its own async `load`, so the memo has
      // to be dropped here — kept, it would hand the repaint below the very spans that are
      // now stale, and the buffer would never colour beyond its first grammar.
      memo = { lang: null, text: null, from: 0, to: 0, spans: null };
      notify(); // repaint with this language now available
    } catch (e) {
      console.error('[bemtvi] failed to load grammar', name, e);
      langs.set(name, null);
    } finally {
      loading.delete(name);
    }
  }

  // Whether `name`'s indents.scm is already available (so a re-`:TSInstall` is a genuine
  // no-op rather than needing to fetch the missing query). An installed grammar's indents
  // live in OPFS next to its parser.wasm; a bundled grammar's only if gen-treesitter
  // vendored one (`indents.json`). A bundled grammar that lacks indents reports false, so
  // `install` falls through and fetches them — see the queries-only path below.
  async function hasIndents(name) {
    if (installed.has(name)) {
      try { return !!(await opfsReadText([...TS_DIR, name, 'indents.scm'])).trim(); }
      catch { return false; }
    }
    return bundledIndents.has(name);
  }

  // Fetch the standard NON-highlights query set (indents/injections/folds/locals) for a
  // grammar from the CDN. `indents` is special — the grammar packages don't ship a usable
  // one, so it comes from nvim-treesitter (the worker indenter consumes it), falling back
  // to the grammar package. The rest are best-effort (a grammar that doesn't ship one is
  // skipped, not an error). Returns `{ kind: text, … }` for whatever was found.
  async function fetchQuerySet(name, cfg) {
    const ver = versionOf(cfg.pkg);
    const extras = {};
    for (const kind of QUERY_KINDS) {
      if (kind === 'highlights') continue;
      // Indents come from nvim-treesitter (the grammar packages don't ship a usable one),
      // with the `; inherits:` chain merged (so `javascript` picks up `ecma`/`jsx`); fall
      // back to the grammar package's own if nvim-treesitter has none.
      if (kind === 'indents') {
        const merged = await fetchQueryMerged(name, 'indents', fetchText, GH_BASE);
        if (merged.trim()) { extras.indents = merged; continue; }
        try { extras.indents = await fetchText(`${CDN_BASE}/${cfg.pkg}@${ver}/queries/indents.scm`); }
        catch { /* no indents from either source */ }
        continue;
      }
      // Folds, like indents, come (inherit-merged) from nvim-treesitter so the browser
      // matches native (the grammar package's own folds.scm, if any, can differ); fall back.
      if (kind === 'folds') {
        const merged = await fetchQueryMerged(name, 'folds', fetchText, GH_BASE);
        if (merged.trim()) { extras.folds = merged; continue; }
        try { extras.folds = await fetchText(`${CDN_BASE}/${cfg.pkg}@${ver}/queries/folds.scm`); }
        catch { /* no folds from either source */ }
        continue;
      }
      // Text objects come from the SEPARATE nvim-treesitter-textobjects repo (neither
      // nvim-treesitter core nor the grammar package ships them), also inherit-merged; the
      // worker's text-object runner (web/ts-textobjects.js) consumes them. Best-effort.
      if (kind === 'textobjects') {
        const merged = await fetchQueryMerged(name, 'textobjects', fetchText, GH_BASE);
        if (merged.trim()) extras.textobjects = merged;
        continue;
      }
      try { extras[kind] = await fetchText(`${CDN_BASE}/${cfg.pkg}@${ver}/queries/${kind}.scm`); }
      catch { /* not shipped by this grammar */ }
    }
    return extras;
  }

  // `:TSInstall <name>` — make a grammar available. If it's already bundled/installed *with
  // its indents.scm*, (re)register it (no network). If it's available but MISSING its
  // indents (the offline bundle ships grammars without indents), fetch just the standard
  // query set and cache it to OPFS next to the existing grammar — no parser.wasm refetch.
  // Otherwise fetch the prebuilt `.wasm` + the standard query set from the CDN, sanitize
  // highlights against the grammar, cache everything in OPFS (so it survives reload), and
  // register it. Returns `{ ok, msg }` for the status line. highlights.scm feeds this UI
  // highlighter; indents.scm is consumed by the worker's tree-sitter indenter
  // (web/ts-indent.js); injections/folds/locals are cached for forward-compat.
  async function install(rawName) {
    await ready;
    // Canonicalize first (`c#`/`csharp`/`cs` → `c_sharp`, …); `name` is what we cache,
    // register, and report back, so OPFS / the installed manifest / `:TSInstallInfo` all
    // use the one canonical spelling regardless of which alias the user typed.
    const name = resolveName(rawName);
    const cfg = REGISTRY[name];
    if (!cfg) return { ok: false, name, msg: `unknown language '${rawName}'` };
    const enc = (s) => new TextEncoder().encode(s);

    // A bundle-only language: its npm package ships no `.wasm`, so ours is compiled from
    // source and committed (`prebuilt` — markdown's two parsers), which also means jsDelivr
    // has no wasm to fetch. There is genuinely nothing to install, so say so instead of
    // 404ing on a URL that cannot exist — and if the vendored copy is missing, fail loud
    // rather than pretending an install could fix it.
    if (cfg.prebuilt) {
      if (!isAvail(name)) {
        return { ok: false, name, msg: `no upstream wasm for '${name}' — it ships in the offline bundle (rebuild web/vendor)` };
      }
      langs.delete(name);
      await load(name);
      return langs.get(name)
        ? { ok: true, name, msg: 'bundled (offline only — upstream ships no wasm)' }
        : { ok: false, name, msg: 'bundled grammar failed to load' };
    }

    // Already available WITH indents — re-register from cache, no network (also the
    // `:TSUpdate` of a complete bundled lang).
    if (isAvail(name) && await hasIndents(name)) {
      langs.delete(name);
      await load(name);
      return langs.get(name)
        ? { ok: true, name, msg: installed.has(name) ? 'already installed' : 'bundled' }
        : { ok: false, name, msg: 'cached grammar failed to load' };
    }

    // Available but missing its indents.scm (a bundled grammar gen-treesitter couldn't
    // fetch indents for, or an older install). The grammar itself is fine — fetch only the
    // standard query set and cache it to OPFS beside the existing (bundled or installed)
    // grammar. The worker indenter prefers an OPFS-cached indents.scm over the absent
    // vendored one (see ts-indent.js), so it picks these up on the post-install reload.
    if (isAvail(name)) {
      try {
        const extras = await fetchQuerySet(name, cfg);
        if (!extras.indents) return { ok: false, name, msg: 'no indents.scm available for this language' };
        for (const [kind, text] of Object.entries(extras)) {
          await opfsWriteBytes([...TS_DIR, name, kind + '.scm'], enc(text));
        }
        // Keep the in-memory highlighter as-is (highlights are unchanged); the indenter
        // reloads off OPFS. memo is cleared so any pending repaint re-runs cleanly.
        memo = { lang: null, text: null, from: 0, to: 0, spans: null };
        notify();
        return { ok: true, name, msg: `queries: ${Object.keys(extras).join('+')}` };
      } catch (e) {
        return { ok: false, name, msg: String(e && e.message ? e.message : e) };
      }
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
      if (caps === 0) { tree.delete(); return { ok: false, name, msg: 'grammar/query mismatch (0 captures)' }; }

      // The rest of the standard query set (indents/injections/folds/locals). Fetched
      // BEFORE the sample tree is released: injections.scm is sanitized against it, the
      // same way highlights just were.
      const extras = await fetchQuerySet(name, cfg);

      // Register with the injections query too (compiled from what we just fetched), so a
      // freshly installed grammar reaches its embedded languages immediately rather than
      // only after a reload re-`load`s it from OPFS.
      let inj = null;
      if (extras.injections && extras.injections.trim()) {
        const ijRes = sanitize(extras.injections, Query, language, tree.rootNode);
        if (ijRes.kept > 0) {
          try { inj = new Query(language, ijRes.text); }
          catch (e) { console.error('[bemtvi] injections query failed to compile for', name, e); }
        }
      }
      tree.delete();

      // Persist to OPFS, then register in memory + the installed manifest.
      await opfsWriteBytes([...TS_DIR, name, 'parser.wasm'], wasmBytes);
      await opfsWriteBytes([...TS_DIR, name, 'highlights.scm'], enc(res.text));
      for (const [kind, text] of Object.entries(extras)) {
        await opfsWriteBytes([...TS_DIR, name, kind + '.scm'], enc(text));
      }
      installed.add(name);
      await opfsWriteBytes([...TS_DIR, 'manifest.json'], enc(JSON.stringify([...installed])));

      langs.set(name, { parser, query, inj });
      memo = { lang: null, text: null, from: 0, to: 0, spans: null }; // a now-installed buffer must re-highlight
      notify();
      const kinds = ['highlights', ...Object.keys(extras)].join('+');
      return { ok: true, name, msg: `${cfg.pkg}@${ver}, queries: ${kinds}` };
    } catch (e) {
      return { ok: false, name, msg: String(e && e.message ? e.message : e) };
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

  // The grammar for a fenced-code info string (the `rust` in ```` ```rust ````), or null
  // when unsupported / not yet available. Takes only the first token (rustdoc writes
  // ```` ```rust,no_run ````) and resolves it through the filetype / extension / install
  // alias tables — so `ts`/`js`/`py`/`sh`/`c#` all reach their grammar. No auto-fetch:
  // `:TSInstall` stays the explicit network gate, exactly like `langForName`.
  function langForFence(info) {
    if (!info || !runtimeReady) return null;
    const k = String(info).toLowerCase().split(/[\s,;]/)[0];
    if (!k) return null;
    for (const cand of [FT[k], EXT[k], resolveName(k), k]) {
      if (cand && isAvail(cand)) return cand;
    }
    return null;
  }

  // Per-line highlight spans for the fenced code blocks in a markdown buffer, found by
  // scanning the text for ```` ```lang ```` fences and parsing each block with that
  // language's OWN grammar (when available, bundled or `:TSInstall`'d).
  //
  // This is the FALLBACK for markdown, not the main path: the markdown grammar is bundled,
  // so `spansForBuffer('markdown', …)` highlights prose, headings and lists *and* reaches
  // the same fenced code through the grammar's own injections. This stays for the window
  // between a markdown buffer appearing and its grammars finishing their async load — the
  // code in a hover is the part worth colouring first — and for a caller with raw markdown
  // and no filetype to key off. Returns per-line spans (same shape as `spansForBuffer`) or
  // null when nothing highlighted.
  function spansForFencedMarkdown(text) {
    if (text == null || !runtimeReady) return null;
    const lines = text.split('\n');
    const out = Array.from({ length: lines.length }, () => []);
    let any = false;
    for (let i = 0; i < lines.length; ) {
      const open = lines[i].match(/^\s*(```+|~~~+)\s*([^\s`~]*)/);
      if (!open) { i++; continue; }
      const closeRe = open[1][0] === '`' ? /^\s*```+\s*$/ : /^\s*~~~+\s*$/;
      let j = i + 1;
      while (j < lines.length && !closeRe.test(lines[j])) j++;
      const lang = langForFence(open[2]);
      if (lang) {
        const sp = spansForBuffer(lang, lines.slice(i + 1, j).join('\n'));
        if (sp) {
          for (let k = 0; k < sp.length && i + 1 + k < out.length; k++) {
            if (sp[k] && sp[k].length) { out[i + 1 + k] = sp[k]; any = true; }
          }
        }
      }
      i = j + 1;
    }
    return any ? out : null;
  }

  // Spans for a RENDERED markdown document whose fences the server already stripped:
  // the code blocks arrive as explicit `{ first_line, len, lang }` row spans (a window's
  // `code_blocks`, from the core's own render) instead of being re-found in the text.
  //
  // This is the doc-float twin of `spansForFencedMarkdown`. That one re-parses ```lang
  // fences out of raw markdown, which is right for a `.md` BUFFER but impossible for a
  // hover float: its fences are gone from the text and its buffer is deliberately
  // untyped, so there is nothing left in the buffer to key off. Here the structure comes
  // from the server, so the text needs no parsing at all — the rows are handed to us.
  function spansForCodeBlocks(text, blocks) {
    if (text == null || !runtimeReady || !blocks || !blocks.length) return null;
    const lines = text.split('\n');
    const out = Array.from({ length: lines.length }, () => []);
    let any = false;
    for (const b of blocks) {
      const lang = langForFence(b.lang || '');
      if (!lang) continue;                       // no fence language, or no such grammar
      const start = b.first_line | 0, len = b.len | 0;
      if (start < 0 || len <= 0 || start >= lines.length) continue;
      const sp = spansForBuffer(lang, lines.slice(start, start + len).join('\n'));
      if (!sp) continue;                         // grammar still loading — leave it plain
      for (let k = 0; k < sp.length && start + k < out.length; k++) {
        if (sp[k] && sp[k].length) { out[start + k] = sp[k]; any = true; }
      }
    }
    return any ? out : null;
  }

  // A tree-sitter metadata/control capture (`@spell` / `@nospell` / `@conceal`) —
  // a spellcheck/conceal marker, NOT a visual highlight group. Grammars tag nodes
  // with these alongside a real highlight (`(comment) @comment @spell`); they carry
  // no colour, so they must not become spans that shadow the highlight they sit
  // beside. Mirrors `is_metadata_capture` in the Rust engine (`bemtvi-ts`).
  function isMetadataCapture(name) {
    const major = name.split('.', 1)[0];
    return major === 'spell' || major === 'nospell' || major === 'conceal';
  }

  // --- Lua-pattern matcher (for `#lua-match?` predicates) ---------------------------
  // A faithful port of Lua 5.4's `string.find(s, p) ~= nil` — the question a tree-
  // sitter `#lua-match?` predicate asks. Lua patterns are NOT regexes: greedy `*`/`+`,
  // lazy `-`, classes `%a %d %s %w %l %u %p %c %x` (+ uppercase complements), sets
  // `[...]`, and the specials `%b` / `%f`. web-tree-sitter only enforces the standard
  // `#match?` predicate, so without this bash's `(#lua-match? … "^#!")` shebang rule
  // leaks `@keyword.directive` onto every comment. Twin of the Rust `lua_pattern`
  // module; operates on UTF-16 code units (exact for the ASCII patterns queries use).
  const L_ESC = 0x25; // '%'
  const LUA_MAX_DEPTH = 200;
  const isDigit = (c) => c >= 0x30 && c <= 0x39;
  const isAlpha = (c) => (c >= 0x41 && c <= 0x5a) || (c >= 0x61 && c <= 0x7a);
  const isHex = (c) => isDigit(c) || (c >= 0x41 && c <= 0x46) || (c >= 0x61 && c <= 0x66);
  // C `isgraph`: printable, non-space ASCII. `ispunct`: graphic and not alphanumeric.
  const isGraph = (c) => c > 0x20 && c < 0x7f;
  const isPunct = (c) => isGraph(c) && !isAlpha(c) && !isDigit(c);

  function luaMatchClass(c, cl) {
    let res;
    switch (cl | 0x20) { // fold to lowercase class letter
      case 0x61: res = isAlpha(c); break;                       // a
      case 0x63: res = c < 0x20 || c === 0x7f; break;           // c (control)
      case 0x64: res = isDigit(c); break;                       // d
      case 0x67: res = isGraph(c); break;                       // g
      case 0x6c: res = c >= 0x61 && c <= 0x7a; break;           // l
      case 0x70: res = isPunct(c); break;                       // p
      case 0x73: res = c === 0x20 || (c >= 0x09 && c <= 0x0d); break; // s (isspace)
      case 0x75: res = c >= 0x41 && c <= 0x5a; break;           // u
      case 0x77: res = isAlpha(c) || isDigit(c); break;         // w
      case 0x78: res = isHex(c); break;                         // x
      default: return cl === c;                                 // a literal escaped char
    }
    return (cl >= 0x41 && cl <= 0x5a) ? !res : res; // uppercase letter complements
  }

  function luaMatch(text, pattern) {
    const anchor = pattern.charCodeAt(0) === 0x5e; // '^'
    const pat = [];
    for (let i = anchor ? 1 : 0; i < pattern.length; i++) pat.push(pattern.charCodeAt(i));
    const src = [];
    for (let i = 0; i < text.length; i++) src.push(text.charCodeAt(i));

    // Index just past the single pattern item at `p` (literal, `%x` class, or `[...]`).
    function classEnd(p) {
      let c = pat[p++];
      if (c === L_ESC) return p < pat.length ? p + 1 : -1;
      if (c === 0x5b) { // '['
        if (pat[p] === 0x5e) p++; // '^'
        do { // first member read unconditionally, so a leading ']' is literal
          if (p >= pat.length) return -1;
          c = pat[p++];
          if (c === L_ESC && p < pat.length) p++;
        } while (pat[p] !== 0x5d); // ']'
        return p + 1;
      }
      return p;
    }
    function matchBracket(c, p, ec) {
      let sig = true;
      if (pat[p + 1] === 0x5e) { sig = false; p++; }
      for (p++; p < ec; ) {
        if (pat[p] === L_ESC) { p++; if (luaMatchClass(c, pat[p])) return sig; p++; }
        else if (pat[p + 1] === 0x2d && p + 2 < ec) { // a range a-z
          if (pat[p] <= c && c <= pat[p + 2]) return sig; p += 3;
        } else { if (pat[p] === c) return sig; p++; }
      }
      return !sig;
    }
    function singleMatch(s, p, ep) {
      if (s >= src.length) return false;
      const c = src[s];
      switch (pat[p]) {
        case 0x2e: return true;                       // '.'
        case L_ESC: return luaMatchClass(c, pat[p + 1]);
        case 0x5b: return matchBracket(c, p, ep - 1); // '['
        default: return pat[p] === c;
      }
    }

    const caps = []; // { init, len } — len -1 unfinished, -2 position
    let depth = LUA_MAX_DEPTH;

    function doMatch(s, p) {
      if (depth-- === 0) { depth++; return -1; }
      const r = doMatchInner(s, p);
      depth++;
      return r;
    }
    function doMatchInner(s, p) {
      for (;;) {
        if (p >= pat.length) return s;
        const pc = pat[p];
        if (pc === 0x28) { // '('
          return pat[p + 1] === 0x29 ? startCapture(s, p + 2, -2) : startCapture(s, p + 1, -1);
        }
        if (pc === 0x29) return endCapture(s, p + 1); // ')'
        if (pc === 0x24 && p + 1 === pat.length) return s === src.length ? s : -1; // '$'
        if (pc === L_ESC) {
          const n = pat[p + 1];
          if (n === 0x62) return matchBalance(s, p + 2);      // %b
          if (n === 0x66) {                                   // %f[set]
            p += 2;
            if (pat[p] !== 0x5b) return -1;
            const ep = classEnd(p);
            if (ep < 0) return -1;
            const prev = s === 0 ? 0 : src[s - 1];
            const curr = s < src.length ? src[s] : 0;
            if (!matchBracket(prev, p, ep - 1) && matchBracket(curr, p, ep - 1)) { p = ep; continue; }
            return -1;
          }
          if (n >= 0x30 && n <= 0x39) {                       // %1..%9 backref
            const ns = matchCapture(s, n);
            if (ns < 0) return -1;
            s = ns; p += 2; continue;
          }
        }
        return defaultMatch(s, p);
      }
    }
    function defaultMatch(s, p) {
      const ep = classEnd(p);
      if (ep < 0) return -1;
      const matched = singleMatch(s, p, ep);
      switch (pat[ep]) {
        case 0x3f: { // '?'
          if (matched) { const r = doMatch(s + 1, ep + 1); if (r >= 0) return r; }
          return doMatch(s, ep + 1);
        }
        case 0x2b: return matched ? maxExpand(s + 1, p, ep) : -1; // '+'
        case 0x2a: return maxExpand(s, p, ep);                    // '*'
        case 0x2d: return minExpand(s, p, ep);                    // '-'
        default: return matched ? doMatch(s + 1, ep) : -1;
      }
    }
    function maxExpand(s, p, ep) {
      let i = 0;
      while (singleMatch(s + i, p, ep)) i++;
      for (;;) { const r = doMatch(s + i, ep + 1); if (r >= 0) return r; if (i === 0) return -1; i--; }
    }
    function minExpand(s, p, ep) {
      for (;;) { const r = doMatch(s, ep + 1); if (r >= 0) return r; if (singleMatch(s, p, ep)) s++; else return -1; }
    }
    function startCapture(s, p, what) {
      caps.push({ init: s, len: what });
      const r = doMatch(s, p);
      if (r < 0) caps.pop();
      return r;
    }
    function endCapture(s, p) {
      let l = -1;
      for (let i = caps.length - 1; i >= 0; i--) if (caps[i].len === -1) { l = i; break; }
      if (l < 0) return -1;
      caps[l].len = s - caps[l].init;
      const r = doMatch(s, p);
      if (r < 0) caps[l].len = -1;
      return r;
    }
    function matchCapture(s, d) {
      const idx = d - 0x31; // '1'
      const cap = caps[idx];
      if (!cap || cap.len < 0) return -1;
      const len = cap.len;
      if (src.length - s < len) return -1;
      for (let k = 0; k < len; k++) if (src[s + k] !== src[cap.init + k]) return -1;
      return s + len;
    }
    function matchBalance(s, p) {
      const b = pat[p], e = pat[p + 1];
      if (b === undefined || e === undefined || src[s] !== b) return -1;
      let cont = 1;
      for (let i = s + 1; i < src.length; i++) {
        if (src[i] === e) { if (--cont === 0) return doMatch(i + 1, p + 2); }
        else if (src[i] === b) cont++;
      }
      return -1;
    }

    let s = 0;
    for (;;) {
      caps.length = 0; depth = LUA_MAX_DEPTH;
      if (doMatch(s, 0) >= 0) return true;
      if (anchor || s >= src.length) return false;
      s++;
    }
  }

  // Whether a capture survives its pattern's `#lua-match?` / `#not-lua-match?`
  // predicates. web-tree-sitter already enforces the standard `#match?` family while
  // collecting captures, but leaves these neovim-specific ones in `predicatesForPattern`
  // for us. Each compares a capture's node text to a Lua pattern; we apply those whose
  // operand names the capture being coloured (the case grammar highlight rules use,
  // e.g. the shebang rule gating its own `@keyword.directive`). Mirrors the Rust engine.
  function captureSatisfiesLuaPredicates(query, capture) {
    const preds = query.predicatesForPattern(capture.patternIndex);
    if (!preds || !preds.length) return true;
    for (const pred of preds) {
      const negate = pred.operator === 'not-lua-match?';
      if (!negate && pred.operator !== 'lua-match?') continue;
      const [capArg, strArg] = pred.operands;
      if (!capArg || capArg.type !== 'capture' || capArg.name !== capture.name) continue;
      if (!strArg || strArg.type !== 'string') continue;
      if (luaMatch(capture.node.text, strArg.value) === negate) return false;
    }
    return true;
  }

  // The `{ lang, start, end }` ranges a grammar's injections.scm hands to OTHER grammars
  // inside `src` — a markdown `(inline)` node to markdown_inline, a ```` ```rust ```` fence
  // to rust. The injected language is named either by an `@injection.language` capture
  // (the fence's info string) or by a `(#set! injection.language "…")` directive, which
  // web-tree-sitter exposes per pattern as `query.setProperties`. Resolution goes through
  // `langForFence`, so an injected name is alias-mapped and — like every other path here —
  // only accepted when its grammar is actually available: an injection into an
  // un-`:TSInstall`ed language yields nothing and that region stays plain.
  //
  // A match with several `@injection.content` captures becomes several ranges, each parsed
  // on its own. That is right for markdown (every `(inline)` node is its own island) but is
  // an approximation of `(#set! injection.combined)`, which asks for ONE parse over the
  // union of the ranges — an html_block split across a markdown list, say, is parsed
  // per-piece here. `(#offset! …)` is not applied either (web-tree-sitter doesn't report
  // it), so a `---` metadata fence is handed to yaml/toml along with its delimiter lines.
  function injectionRanges(query, root, src) {
    const out = [];
    for (const m of query.matches(root)) {
      const props = query.setProperties[m.patternIndex] || {};
      const named = m.captures.find((c) => c.name === 'injection.language');
      const raw = named ? src.slice(named.node.startIndex, named.node.endIndex) : props['injection.language'];
      const lang = langForFence(raw);
      if (!lang) continue;
      for (const c of m.captures) {
        if (c.name !== 'injection.content') continue;
        const start = c.node.startIndex, end = c.node.endIndex;
        if (end > start) out.push({ lang, start, end });
      }
    }
    return out;
  }

  // Parse `src` with `name`'s grammar and push its captures into `ctx.spans`, shifting
  // every index by `base` — the offset of `src` within the buffer (0 for the buffer's own
  // language, the injected range's start for embedded content), so an injected token lands
  // on the buffer line it actually occupies. Then recurse through this grammar's
  // injections, which is what colours a markdown buffer's prose (via markdown_inline) and
  // the code inside its fences (via that fence's own grammar).
  function paintLang(name, src, base, ctx, depth) {
    const entry = langs.get(name);
    if (!entry) return;
    const tree = entry.parser.parse(src);
    for (const c of entry.query.captures(tree.rootNode)) {
      // Drop metadata captures (`@spell`/…), full-line-background groups, and captures
      // whose `#lua-match?` predicate fails, at the source — so they never become spans
      // that shadow a real highlight (this mirrors the server engine's `extract_spans`).
      if (isMetadataCapture(c.name) || LINE_BACKGROUND_CAPTURES.has(c.name)) continue;
      if (!captureSatisfiesLuaPredicates(entry.query, c)) continue;
      const s = c.node.startIndex + base, e = c.node.endIndex + base;
      if (e <= s) continue;
      const endLn = ctx.lineAt(e - 1);
      for (let ln = ctx.lineAt(s); ln <= endLn; ln++) {
        const lb = ctx.lineStart[ln], le = lb + ctx.lines[ln].length;
        const a = Math.max(s, lb) - lb, b = Math.min(e, le) - lb;
        if (b > a) ctx.spans[ln].push([a, b, c.name]);
      }
    }
    if (entry.inj && depth < MAX_INJECTION_DEPTH) {
      for (const r of injectionRanges(entry.inj, tree.rootNode, src)) {
        // Outside the band being painted (see INJECTION_BAND): nothing would be rendered
        // from it, so don't pay for the parse.
        if (ctx.lineAt(base + r.end - 1) < ctx.from || ctx.lineAt(base + r.start) > ctx.to) continue;
        // Its grammar is available but may not be compiled in memory yet; `load` is async,
        // so this region paints on the repaint its `onReady` triggers — exactly how the
        // buffer's own language behaves on first sight.
        if (!langs.has(r.lang)) { load(r.lang); continue; }
        paintLang(r.lang, src.slice(r.start, r.end), base + r.start, ctx, depth + 1);
      }
    }
    tree.delete();
  }

  // Per-buffer-line capture spans for `text` in `lang`: an array indexed by 0-based
  // buffer line, each entry a list of `[startU16, endU16, group]`. Returns null when
  // the grammar isn't loaded yet — and kicks off the async load, after which onReady
  // fires a repaint. Multi-line captures (block comments, here-docs) are split at
  // line boundaries so each line carries only its own slice.
  //
  // Embedded content is included: `paintLang` follows the grammar's injections into the
  // buffer's own lines. `visible` — `{ from, to }` in 0-based buffer lines, the caller's
  // viewport — bounds THAT work to a band around those lines (see `INJECTION_BAND`); the
  // language's own captures always cover the whole buffer. Omit it (a small buffer: a
  // preview, a float) to inject everywhere.
  function spansForBuffer(lang, text, visible) {
    if (!lang) return null;
    if (!langs.has(lang)) { load(lang); return null; }
    if (!langs.get(lang)) return null; // load failed earlier
    const from = visible ? Math.max(0, Math.floor(visible.from / INJECTION_BAND - 1) * INJECTION_BAND) : 0;
    const to = visible ? Math.ceil(visible.to / INJECTION_BAND + 1) * INJECTION_BAND : Infinity;
    if (memo.lang === lang && memo.text === text && memo.from === from && memo.to === to) return memo.spans;

    const lines = text.split('\n');
    const lineStart = new Array(lines.length);
    for (let i = 0, off = 0; i < lines.length; i++) { lineStart[i] = off; off += lines[i].length + 1; }
    const ctx = {
      spans: Array.from({ length: lines.length }, () => []),
      lines,
      lineStart,
      from,
      to,
      lineAt: (idx) => {
        let lo = 0, hi = lines.length - 1;
        while (lo < hi) { const mid = (lo + hi + 1) >> 1; if (lineStart[mid] <= idx) lo = mid; else hi = mid - 1; }
        return lo;
      },
    };
    paintLang(lang, text, 0, ctx, 0);
    memo = { lang, text, from, to, spans: ctx.spans };
    return ctx.spans;
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
    spansForFencedMarkdown,
    spansForCodeBlocks,
    colorsForLine,
    install,
  };
}
