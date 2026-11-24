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

import { createScmLoader, NEED_TEXT, byteColumns, u16Col } from './ts-buffers.js';

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

// Build the runner over the shared buffer store (ts-buffers.js): the buffer's text and
// tree come from the store, kept current from the core's edit deltas, so a text-object
// ask neither copies nor re-parses the buffer. The cursor arrives as (row, byte column)
// and regions go back as (row, byte column) pairs, which the core turns into byte
// offsets against its own rope: no conversion here walks the whole text.
export function createTextObjects(store) {
  const scm = createScmLoader(store, 'textobjects', 'textobjects');
  return {
    pendingLoads: () => scm.pendingLoads(),
    ensureForFrame: (frame) => scm.ensureForFrame(frame),
    // Whether tree-sitter text objects are available for `coreLang` (a grammar with a
    // textobjects.scm is loaded). Kicks off a load on first miss.
    available: (coreLang) => scm.available(coreLang),

    // The regions of buffer `buf` (`coreLang`, at the core's text `version`) captured as
    // `capture` that contain the cursor at (row, byteCol), innermost first, each as
    // `[startRow, startByteCol, endRow, endByteCol]` — or `NEED_TEXT` when the store
    // needs the text, or null for the fallback cases (grammar still loading, no
    // grammar / textobjects.scm). Synchronous.
    textObjectsAt(buf, version, coreLang, getText, capture, row, byteCol) {
      const reg = store.resolveReg(coreLang);
      if (!reg) return null;
      const e = scm.entry(reg);
      if (!e) return null;
      const b = store.buffer(buf, version, reg, e.language, getText);
      if (b === NEED_TEXT) return NEED_TEXT;
      if (b.failed) return null;
      if (store.parse(buf, b, { complete: true }) !== 'done') return null;
      try {
        const line = b.lines[row];
        if (line === undefined) return [];
        const u16Cursor = b.lens.prefix(row) + u16Col(line, byteCol);
        const regions = regionsAt(e.query, b.tree.rootNode, capture, u16Cursor);
        if (!regions.length) return []; // available, but nothing surrounds the cursor
        // Each bound as (row, UTF-16 column), then byte columns per row.
        const cols = new Map();
        // An index at the very end of the text maps one row past the last: clamp it to
        // the end of the last line.
        const pos = (i) => {
          let [r, c] = b.lens.find(i);
          if (r >= b.lines.length) { r = b.lines.length - 1; c = b.lines[r].length; }
          return [r, c];
        };
        const at = regions.map(([s, t]) => [pos(s), pos(t)]);
        for (const pair of at) {
          for (const [r, c] of pair) {
            if (!cols.has(r)) cols.set(r, new Set());
            cols.get(r).add(c);
          }
        }
        const bytes = byteColumns(b, cols);
        return at.map(([[sr, sc], [er, ec]]) => [sr, bytes.get(sr).get(sc), er, bytes.get(er).get(ec)]);
      } catch (err) {
        console.error('[bemtvi] textobjects: query failed for', reg, err);
        return null;
      }
    },

    // Evict `coreLang`'s textobjects.scm after a `:TSInstall` so the next ask reloads it.
    reload: (coreLang) => scm.reload(coreLang),
  };
}
