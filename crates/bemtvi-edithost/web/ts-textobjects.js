// In-worker tree-sitter TEXT OBJECTS for the bemtvi edit-host.
//
// Like indentation (ts-indent.js) and folds (ts-folds.js) — and unlike highlighting,
// a UI-thread overlay — text objects must be resolved *synchronously, inside the
// editor tick*: when `vif` / `daf` / `dia` runs, the core asks "what byte range does
// this object cover at the cursor?" mid-keystroke. The tick runs in THIS Web Worker,
// so the answer comes from here. web-tree-sitter's parse + query are synchronous once
// a grammar is loaded; this module loads grammars ahead of time and answers
// `textObjectsAt()` synchronously, reached from the Rust core through the
// `eh_js_ts_textobjects*` FFI bridge (web/eh-lib.js → WasmSyntax in src/lib.rs).
//
// The object rules are nvim-treesitter-textobjects' `textobjects.scm` (`@function.inner`
// /`@function.outer`, `@parameter.*`, `@class.*`, `@comment.*`, `@loop.*`, `@call.*`, …
// captures). The range logic below is a faithful port of bemtvi-ts's
// `engine.rs::text_objects_at`: run the query, **union** the nodes captured under the
// requested name within each match (an inner region can span several statements —
// `_+ @function.inner`), keep the regions that CONTAIN the cursor, innermost (smallest)
// first. The core then picks the count-th region and applies the operator.
//
// Coordinates: web-tree-sitter reports node indices in UTF-16 code units (JS string
// indices); the core is UTF-8 byte oriented. This module converts the incoming cursor
// byte offset to UTF-16 for the query, and the resulting region indices back to bytes —
// so a buffer with non-ASCII text still resolves correctly (ts-indent's "column == byte"
// shortcut does not hold for arbitrary object ranges).

import { EXT, FT, REGISTRY } from './grammars.js';

// web-tree-sitter is imported dynamically (inside init), guarded: a failure leaves text
// objects unavailable (the core falls back to nothing) rather than aborting the worker.
let Parser, Language, Query;

// Vendored assets live next to this module (web/vendor/), same as ts-folds.js.
const V = new URL('./vendor/', import.meta.url);
// OPFS path where `:TSInstall` caches grammars: /.bemtvi/treesitter/<lang>/{parser.wasm,textobjects.scm}.
const TS_DIR = ['.bemtvi', 'treesitter'];

// --- OPFS (worker thread) -----------------------------------------------------------
async function opfsDir(parts) {
  let dir = await navigator.storage.getDirectory();
  for (const p of parts) dir = await dir.getDirectoryHandle(p, { create: false });
  return dir;
}
async function opfsReadBytes(parts) {
  const dir = await opfsDir(parts.slice(0, -1));
  const fh = await dir.getFileHandle(parts[parts.length - 1], { create: false });
  return new Uint8Array(await (await fh.getFile()).arrayBuffer());
}
async function opfsReadText(parts) {
  return new TextDecoder().decode(await opfsReadBytes(parts));
}
async function fetchText(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${r.status} fetching ${url}`);
  return r.text();
}

// --- UTF-8 byte ↔ UTF-16 index conversion -------------------------------------------
// UTF-8 byte length of a code point.
function cpBytes(cp) {
  return cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
}

// A UTF-8 byte offset into `s` → its UTF-16 (JS string) index.
function byteToU16(s, byte) {
  let b = 0;
  for (let i = 0; i < s.length; ) {
    if (b >= byte) return i;
    const cp = s.codePointAt(i);
    b += cpBytes(cp);
    i += cp > 0xffff ? 2 : 1;
  }
  return s.length;
}

// Several UTF-16 indices → their UTF-8 byte offsets, in ONE pass over `s`.
// Returns a Map(u16 -> byte). Node indices are code-point-aligned, so each target lands
// exactly on a step boundary.
function u16ToBytes(s, wanted) {
  const targets = [...new Set(wanted)].sort((a, b) => a - b);
  const out = new Map();
  let ti = 0, b = 0, i = 0;
  while (ti < targets.length) {
    while (ti < targets.length && targets[ti] <= i) {
      out.set(targets[ti], b);
      ti++;
    }
    if (i >= s.length) break;
    const cp = s.codePointAt(i);
    b += cpBytes(cp);
    i += cp > 0xffff ? 2 : 1;
  }
  while (ti < targets.length) {
    out.set(targets[ti], b); // any target at/after end → total byte length
    ti++;
  }
  return out;
}

// The regions (UTF-16 [start, end) pairs) captured as `captureName` that CONTAIN
// `u16Cursor`, innermost (smallest span) first — a port of engine.rs::text_objects_at.
function regionsAt(query, root, captureName, u16Cursor) {
  const regions = [];
  for (const m of query.matches(root)) {
    // Union all nodes this match captures under `captureName` — an inner object can
    // span several nodes (`_+ @function.inner`); the region is their combined extent.
    let lo = Infinity, hi = -1;
    for (const c of m.captures) {
      if (c.name !== captureName) continue;
      if (c.node.startIndex < lo) lo = c.node.startIndex;
      if (c.node.endIndex > hi) hi = c.node.endIndex;
    }
    if (lo < hi && lo <= u16Cursor && u16Cursor < hi) regions.push([lo, hi]);
  }
  regions.sort((a, b) => a[1] - a[0] - (b[1] - b[0]) || a[0] - b[0]);
  // Dedup adjacent identical spans (a region two patterns produce the same way).
  const out = [];
  for (const r of regions) {
    const last = out[out.length - 1];
    if (!last || last[0] !== r[0] || last[1] !== r[1]) out.push(r);
  }
  return out;
}

// Build the runner. `createTextObjects()` mirrors ts-folds.js's `createFolder()`: the
// worker installs its methods on globalThis for the FFI bridge and warms grammars each
// redraw.
export function createTextObjects() {
  const bundled = new Set();     // languages with a grammar vendored in ./vendor/
  const bundledTo = new Set();   // bundled languages that also vendor a textobjects.scm
  const installed = new Set();   // languages cached in OPFS by `:TSInstall`
  const langs = new Map();       // regName → { parser, query } once loaded, or null if unavailable
  const loading = new Set();

  let ready = false;
  let readyResolve;
  const readyP = new Promise((r) => { readyResolve = r; });

  (async () => {
    ({ Parser, Language, Query } = await import('./vendor/web-tree-sitter/web-tree-sitter.js'));
    await Parser.init({ locateFile: (f) => new URL('web-tree-sitter/' + f, V).href });
    try {
      const res = await fetch(new URL('manifest.json', V));
      if (res.ok) for (const l of (await res.json()).languages || []) bundled.add(l);
    } catch (e) { console.error('[bemtvi] textobjects: vendor manifest load failed:', e); }
    try {
      const res = await fetch(new URL('textobjects.json', V));
      if (res.ok) for (const l of await res.json()) bundledTo.add(l);
    } catch { /* no bundled textobjects */ }
    await refreshInstalled();
    ready = true;
    readyResolve();
  })().catch((e) => console.error('[bemtvi] textobjects: init failed:', e && e.stack ? e.stack : e));

  async function refreshInstalled() {
    try {
      for (const l of JSON.parse(await opfsReadText([...TS_DIR, 'manifest.json']))) installed.add(l);
    } catch { /* nothing installed yet */ }
  }

  // The core's filetype → a registry grammar name (mirrors ts-folds.js), or null.
  function resolveReg(coreLang) {
    if (!coreLang) return null;
    const reg = FT[coreLang] || coreLang;
    return REGISTRY[reg] ? reg : null;
  }

  // Load `reg`'s grammar + textobjects.scm into memory (idempotent). An installed copy
  // wins over the bundle. `langs[reg] = null` for any honest "no ts text objects here"
  // case (no grammar, no textobjects.scm, or a load/compile failure) → the caller falls back.
  async function ensure(reg) {
    if (langs.has(reg) || loading.has(reg)) return;
    loading.add(reg);
    try {
      if (!ready) await readyP;
      let parserSrc, toScm;
      if (installed.has(reg)) {
        parserSrc = await opfsReadBytes([...TS_DIR, reg, 'parser.wasm']);
        try { toScm = await opfsReadText([...TS_DIR, reg, 'textobjects.scm']); } catch { toScm = null; }
      } else if (bundled.has(reg)) {
        parserSrc = new URL('grammars/' + reg + '.wasm', V).href;
        // Prefer an OPFS-cached textobjects.scm (a `:TSInstall` that supplied what the
        // offline bundle lacked) over the vendored one (or none).
        try { toScm = await opfsReadText([...TS_DIR, reg, 'textobjects.scm']); }
        catch { toScm = bundledTo.has(reg) ? await fetchText(new URL('textobjects/' + reg + '.scm', V).href) : null; }
      } else {
        langs.set(reg, null);
        return;
      }
      if (!toScm || !toScm.trim()) { langs.set(reg, null); return; } // no query → fall back
      const language = await Language.load(parserSrc);
      const query = new Query(language, toScm);
      const parser = new Parser();
      parser.setLanguage(language);
      langs.set(reg, { parser, query });
    } catch (e) {
      console.error('[bemtvi] textobjects: failed to load', reg, e);
      langs.set(reg, null);
    } finally {
      loading.delete(reg);
    }
  }

  return {
    // How much async work is in flight (in-flight loads + the one-time init): the worker
    // keeps its run loop event-loop-live while > 0, like the indenter / folder.
    pendingLoads() {
      return loading.size + (ready ? 0 : 1);
    },

    // Warm the grammars for the languages on screen, so objects are ready before use.
    ensureForFrame(frame) {
      if (!frame || !Array.isArray(frame.windows)) return;
      for (const w of frame.windows) {
        let reg = resolveReg(w.filetype);
        if (!reg && w.file_name && !w.unnamed) {
          const base = String(w.file_name).split(/[\\/]/).pop().toLowerCase();
          const dot = base.lastIndexOf('.');
          reg = REGISTRY[EXT[dot >= 0 ? base.slice(dot + 1) : '']] ? EXT[base.slice(dot + 1)] : null;
        }
        if (reg) ensure(reg);
      }
    },

    // Whether tree-sitter text objects are available for `coreLang` (a grammar with a
    // textobjects.scm is loaded). Kicks off a load on first miss.
    available(coreLang) {
      const reg = resolveReg(coreLang);
      if (!reg) return false;
      const entry = langs.get(reg);
      if (entry === undefined) { ensure(reg); return false; }
      return !!entry;
    },

    // Byte ranges `[start, end)` of `text`'s `capture`-captured objects that contain
    // byte offset `byte`, innermost first — or null for the fallback cases (grammar
    // still loading, no grammar / textobjects.scm). Synchronous.
    textObjectsAt(coreLang, text, capture, byte) {
      const reg = resolveReg(coreLang);
      if (!reg) return null;
      const entry = langs.get(reg);
      if (entry === undefined) { ensure(reg); return null; } // warm + fall back
      if (!entry) return null;
      const tree = entry.parser.parse(text);
      try {
        const u16Cursor = byteToU16(text, byte);
        const regions = regionsAt(entry.query, tree.rootNode, capture, u16Cursor);
        if (!regions.length) return []; // available, but nothing surrounds the cursor
        const bounds = [];
        for (const [s, e] of regions) bounds.push(s, e);
        const map = u16ToBytes(text, bounds);
        return regions.map(([s, e]) => [map.get(s), map.get(e)]);
      } catch (e) {
        console.error('[bemtvi] textobjects: query failed for', reg, e);
        return null;
      } finally {
        tree.delete();
      }
    },

    // Evict `coreLang`'s cached grammar after a `:TSInstall` so the next query reloads it.
    reload(coreLang) {
      const reg = resolveReg(coreLang) || (coreLang && (FT[coreLang] || coreLang));
      if (reg) langs.delete(reg);
      refreshInstalled();
    },
  };
}
