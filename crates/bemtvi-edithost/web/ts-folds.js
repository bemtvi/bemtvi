// In-worker tree-sitter FOLDS for the bemtvi edit-host (the browser twin of
// bemtvi-ts's `Engine::folds`).
//
// Like indentation (ts-indent.js) and unlike highlighting, the fold structure is
// decided *inside the editor tick*: when `foldmethod=expr` + the tree-sitter
// foldexpr is on, the core asks "what are this buffer's foldable ranges?" while
// converging a keystroke, and the tick runs in THIS Web Worker — so the answer
// must come from here, synchronously. web-tree-sitter's parse + query are
// synchronous once a grammar is loaded, so this module loads grammars ahead of
// time (async) and then answers `folds()` synchronously, which the Rust core
// reaches through the `eh_js_ts_folds*` FFI bridge (web/eh-lib.js → lib.rs's
// `WasmSyntax::folds`).
//
// The fold rules are nvim-treesitter's `folds.scm` (`@fold` captures); the core
// turns the reported node ranges into per-line levels by containment, exactly as
// native does (crates/bemtvi-core/src/editor/fold.rs). Fold queries come from
// nvim-treesitter (bundled offline by gen-treesitter, or cached in OPFS by
// `:TSInstall`), the same source native reads — so the browser folds match native.
//
// This mirrors ts-indent.js's grammar-loading scaffolding (vendor + OPFS install
// caches, lazy per-language load) — it just loads `folds.scm` instead of
// `indents.scm` and returns ranges instead of an indent width.

import { EXT, FT, REGISTRY } from './grammars.js';

// web-tree-sitter is imported *dynamically* (inside init), guarded: a failure
// leaves ts-folds unavailable (the core simply has no tree-sitter folds) rather
// than aborting the worker. Filled in by `createFolder`'s init.
let Parser, Language, Query;

// Vendored assets live next to this module (web/vendor/), same as ts-indent.js.
const V = new URL('./vendor/', import.meta.url);
// OPFS path where `:TSInstall` caches grammars: /.bemtvi/treesitter/<lang>/{parser.wasm,folds.scm}.
const TS_DIR = ['.bemtvi', 'treesitter'];

// --- OPFS (worker thread) — same helpers as ts-indent.js ----------------------------
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

// Foldable line ranges for the parsed `root`: each `@fold`-captured node's
// `[startRow, endRow]` inclusive span. A node whose range ends at column 0 of its
// last line doesn't really occupy that line (its closer is on the line above), so
// trim it — matching bemtvi-ts's `Engine::folds` (and neovim's foldexpr).
function foldRanges(query, root) {
  const out = [];
  for (const m of query.matches(root)) {
    for (const c of m.captures) {
      if (c.name !== 'fold') continue; // only @fold defines folds
      const node = c.node;
      const start = node.startPosition.row;
      let end = node.endPosition.row;
      if (end > start && node.endPosition.column === 0) end -= 1;
      out.push([start, end]);
    }
  }
  return out;
}

// Build the worker's fold runner. Mirrors ts-indent.js's `createIndenter`: it loads
// grammars (bundled vendor / OPFS `:TSInstall` caches) ahead of the keystroke and
// answers `folds`/`available`/`reload` synchronously. The Worker installs the
// returned methods on globalThis for the FFI bridge and warms grammars each redraw.
export function createFolder() {
  const bundled = new Set();      // languages with a grammar vendored in ./vendor/
  const bundledFolds = new Set(); // bundled languages that also vendor a folds.scm
  const installed = new Set();    // languages cached in OPFS by `:TSInstall`
  const langs = new Map();        // regName → { parser, query } once loaded, or null if unavailable
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
    } catch (e) { console.error('[bemtvi] folder: vendor manifest load failed:', e); }
    try {
      const res = await fetch(new URL('folds.json', V));
      if (res.ok) for (const l of await res.json()) bundledFolds.add(l);
    } catch { /* no bundled folds */ }
    await refreshInstalled();
    ready = true;
    readyResolve();
  })().catch((e) => console.error('[bemtvi] folder: init failed:', e && e.stack ? e.stack : e));

  async function refreshInstalled() {
    try {
      for (const l of JSON.parse(await opfsReadText([...TS_DIR, 'manifest.json']))) installed.add(l);
    } catch { /* nothing installed yet */ }
  }

  // The core's filetype → a registry grammar name (mirrors ts-indent.js), or null.
  function resolveReg(coreLang) {
    if (!coreLang) return null;
    const reg = FT[coreLang] || coreLang;
    return REGISTRY[reg] ? reg : null;
  }

  // Load `reg`'s grammar + folds.scm into memory (idempotent). An installed copy wins
  // over the bundle. Sets `langs[reg] = null` for any honest "no ts folds here" case
  // (no grammar, no folds.scm, or a load/compile failure) so the caller falls back.
  async function ensure(reg) {
    if (langs.has(reg) || loading.has(reg)) return;
    loading.add(reg);
    try {
      if (!ready) await readyP;
      let parserSrc, foldScm;
      if (installed.has(reg)) {
        parserSrc = await opfsReadBytes([...TS_DIR, reg, 'parser.wasm']);
        try { foldScm = await opfsReadText([...TS_DIR, reg, 'folds.scm']); } catch { foldScm = null; }
      } else if (bundled.has(reg)) {
        parserSrc = new URL('grammars/' + reg + '.wasm', V).href;
        // Prefer an OPFS-cached folds.scm (a `:TSInstall` that supplied the folds the
        // offline bundle lacks for this grammar) over the vendored one (or none).
        try { foldScm = await opfsReadText([...TS_DIR, reg, 'folds.scm']); }
        catch { foldScm = bundledFolds.has(reg) ? await fetchText(new URL('folds/' + reg + '.scm', V).href) : null; }
      } else {
        langs.set(reg, null);
        return;
      }
      if (!foldScm || !foldScm.trim()) { langs.set(reg, null); return; } // no folds → fall back
      const language = await Language.load(parserSrc);
      const query = new Query(language, foldScm);
      const parser = new Parser();
      parser.setLanguage(language);
      langs.set(reg, { parser, query });
    } catch (e) {
      console.error('[bemtvi] folder: failed to load', reg, e);
      langs.set(reg, null);
    } finally {
      loading.delete(reg);
    }
  }

  return {
    // How much async work is in flight (in-flight loads + the one-time init): the
    // worker keeps its run loop event-loop-live while > 0, exactly like the indenter.
    pendingLoads() {
      return loading.size + (ready ? 0 : 1);
    },

    // Warm the grammars for the languages on screen, so folds are ready before the
    // user enables them. Called from the worker each redraw with the current frame.
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

    // Whether tree-sitter folds are available for `coreLang` (a grammar with a
    // folds.scm is loaded) — the core reads this to tell "loaded, no folds found"
    // from "grammar not ready yet". Kicks off a load on first miss.
    available(coreLang) {
      const reg = resolveReg(coreLang);
      if (!reg) return false;
      const entry = langs.get(reg);
      if (entry === undefined) { ensure(reg); return false; }
      return !!entry;
    },

    // Foldable ranges for `text` in `coreLang` — an array of `[startRow, endRow]`
    // inclusive pairs, or null for the fallback cases (grammar still loading, no
    // grammar/folds). Synchronous: parse + query both are.
    folds(coreLang, text) {
      const reg = resolveReg(coreLang);
      if (!reg) return null;
      const entry = langs.get(reg);
      if (entry === undefined) { ensure(reg); return null; } // not loaded yet — warm + fall back
      if (!entry) return null;
      const tree = entry.parser.parse(text);
      try {
        return foldRanges(entry.query, tree.rootNode);
      } catch (e) {
        console.error('[bemtvi] folder: folds failed for', reg, e);
        return null;
      } finally {
        tree.delete();
      }
    },

    // Evict `coreLang`'s cached grammar after a `:TSInstall` so the next query reloads
    // it (picking up the freshly cached parser + folds.scm).
    reload(coreLang) {
      const reg = resolveReg(coreLang) || (coreLang && (FT[coreLang] || coreLang));
      if (reg) langs.delete(reg);
      refreshInstalled();
    },
  };
}
