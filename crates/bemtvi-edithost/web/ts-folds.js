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

import { createScmLoader, NEED_TEXT } from './ts-buffers.js';

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

// Build the worker's fold runner over the shared buffer store (ts-buffers.js): the
// buffer's text and tree come from the store, kept current from the core's edit deltas,
// so a fold ask neither copies nor re-parses the buffer. The Worker installs the
// returned methods on globalThis for the FFI bridge and warms grammars each redraw.
export function createFolder(store) {
  const scm = createScmLoader(store, 'folds', 'folder');
  return {
    pendingLoads: () => scm.pendingLoads(),
    ensureForFrame: (frame) => scm.ensureForFrame(frame),
    // Whether tree-sitter folds are available for `coreLang` (a grammar with a
    // folds.scm is loaded) — the core reads this to tell "loaded, no folds found" from
    // "grammar not ready yet". Kicks off a load on first miss.
    available: (coreLang) => scm.available(coreLang),

    // Foldable ranges for buffer `buf` (`coreLang`, at the core's text `version`) — an
    // array of `[startRow, endRow]` inclusive pairs, `NEED_TEXT` when the store needs
    // the text (`getText()` gave none), or null for the fallback cases (grammar still
    // loading, no grammar/folds: the core asks again later). Synchronous.
    folds(buf, version, coreLang, getText) {
      const reg = store.resolveReg(coreLang);
      if (!reg) return null;
      const e = scm.entry(reg);
      if (!e) return null;
      const b = store.buffer(buf, version, reg, e.language, getText);
      if (b === NEED_TEXT) return NEED_TEXT;
      if (b.failed) return null;
      if (store.parse(buf, b, { complete: true }) !== 'done') return null;
      try {
        return foldRanges(e.query, b.tree.rootNode);
      } catch (err) {
        console.error('[bemtvi] folder: folds failed for', reg, err);
        return null;
      }
    },

    // Evict `coreLang`'s folds.scm after a `:TSInstall` so the next ask reloads it.
    reload: (coreLang) => scm.reload(coreLang),
  };
}
