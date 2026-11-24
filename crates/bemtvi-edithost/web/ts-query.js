// In-worker tree-sitter QUERIES for the bemtvi edit-host: the browser half of
// `btv.treesitter.query` and the `query` key on `btv.decor.provider`.
//
// Answered *synchronously, inside the editor tick* (a decor provider's `ctx.query` is
// filled while the dispatch builds its ctx), reached from the Rust core through the
// `eh_js_ts_query` FFI bridge (web/eh-lib.js → WasmSyntax::query in src/lib.rs). A port
// of bemtvi-ts's `engine.rs::Engine::query`: run the caller's query over the buffer's
// root tree, restricted to a row range, and report every captured node with its full
// extent.
//
// The text, parser and tree live in the shared buffer store (ts-buffers.js), kept
// current from the core's edit deltas, so a query costs nothing in proportion to the
// buffer. Walking the tree, and converting the captures' columns, are bounded by the
// caller's deadline; the answer by MAX_QUERY_CAPTURES. A capture's byte column comes
// from its own row.

import { byteColumns } from './ts-buffers.js';

// The most captured nodes one query returns, as in engine.rs. Past it the query fails
// loud rather than handing back a truncated list.
const MAX_QUERY_CAPTURES = 100000;
// Compiled queries kept per runner; past this the cache starts over.
const MAX_COMPILED = 32;

const answer = (o) => JSON.stringify(o);

// A deadline check cheap enough to call in a hot loop: reads the clock every 4096 calls.
function deadlineCheck(until) {
  let n = 0;
  return () => (++n & 4095) === 0 && performance.now() > until;
}

// Build the runner over the shared `store`.
export function createQueryRunner(store) {
  const queries = new Map(); // `${reg}\0${src}` → Query | { error: msg }

  function compile(reg, language, src) {
    const key = reg + '\0' + src;
    let q = queries.get(key);
    if (q === undefined) {
      try { q = new store.api.Query(language, src); }
      catch (e) { q = { error: String(e && e.message ? e.message : e) }; }
      if (queries.size >= MAX_COMPILED) {
        for (const old of queries.values()) if (old.delete) old.delete();
        queries.clear();
      }
      queries.set(key, q);
    }
    return q;
  }

  function run(buf, version, coreLang, getText, src, first, lastRow, deadlineMs, resume) {
    if (store.initError) return answer({ err: 'failed', msg: store.initError });
    if (!store.ready) { store.language(null); return answer({ err: 'loading' }); }
    const reg = store.resolveReg(coreLang);
    if (!reg) return answer({ err: 'nogrammar' });
    const entry = store.language(reg);
    if (entry === undefined) return answer({ err: 'loading' });
    if (entry === null) return answer({ err: 'nogrammar' });
    if (entry.failed) return answer({ err: 'failed', msg: entry.failed });
    const q = compile(reg, entry.language, src);
    if (q.error) return answer({ err: 'compile', msg: q.error });
    // An empty row range matches nothing (after the grammar and compile checks, so a
    // broken query still says so); tree-sitter would read an end of (0, 0) as "no
    // limit", so it must not reach the cursor.
    if (first >= 0 && lastRow <= first) return answer({ ok: [] });

    const b = store.buffer(buf, version, reg, entry.language, getText);
    if (typeof b === 'string') return b; // NEED_TEXT
    if (b.failed) return answer({ err: 'failed', msg: b.failed });
    // A query that may advance the parse runs one slice; either way one that finds it
    // unfinished is told "not yet" and woken when it finishes.
    const state = store.parse(buf, b, { sliceMs: resume ? 50 : 0, owe: true });
    if (state === 'parsing') return answer({ err: 'incomplete' });
    if (state !== 'done') return answer({ err: 'failed', msg: state.failed });

    const until = performance.now() + deadlineMs;
    let timedOut = false;
    const opts = {
      progressCallback: () => {
        if (performance.now() > until) { timedOut = true; return true; }
        return false;
      },
    };
    if (first >= 0) {
      opts.startPosition = { row: first, column: 0 };
      opts.endPosition = { row: lastRow, column: 0 };
    }
    const matches = q.matches(b.tree.rootNode, opts);
    if (timedOut) return answer({ err: 'timeout', n: deadlineMs });

    // The UTF-16 columns whose byte columns we need, per row.
    let captured = 0;
    const cols = new Map(); // row → Set of UTF-16 columns
    const want = (row, col) => {
      let set = cols.get(row);
      if (!set) { set = new Set(); cols.set(row, set); }
      set.add(col);
    };
    for (const m of matches) {
      captured += m.captures.length;
      if (captured > MAX_QUERY_CAPTURES) return answer({ err: 'toomany', n: MAX_QUERY_CAPTURES });
      for (const c of m.captures) {
        want(c.node.startPosition.row, c.node.startPosition.column);
        want(c.node.endPosition.row, c.node.endPosition.column);
      }
    }
    const bytes = byteColumns(b, cols, deadlineCheck(until));
    if (!bytes) return answer({ err: 'timeout', n: deadlineMs });

    const out = [];
    for (const m of matches) {
      const caps = [];
      for (const c of m.captures) {
        const n = c.node;
        const sp = n.startPosition, ep = n.endPosition;
        caps.push([c.name, n.type, sp.row, bytes.get(sp.row).get(sp.column), ep.row, bytes.get(ep.row).get(ep.column)]);
      }
      out.push([m.patternIndex, caps]);
    }
    return answer({ ok: out });
  }

  return {
    // Run `src` over buffer `buf` (`coreLang`, at the core's text `version`), restricted
    // to rows [first, last) when `first >= 0`, walking the tree for at most
    // `deadlineMs`. With `resume`, an unfinished parse runs one slice first. Returns a
    // JSON string the core decodes (or `NEED_TEXT`, asking for the text):
    //   {"ok": [[pattern, [[name, kind, row, col, endRow, endCol], …]], …]}
    //   {"err": "loading" | "incomplete" | "nogrammar" | "failed" | "compile" |
    //           "toomany" | "timeout", "msg"?: …, "n"?: …}
    // Columns are UTF-8 bytes. Synchronous.
    query(buf, version, coreLang, getText, src, first, lastRow, deadlineMs, resume) {
      try { return run(buf, version, coreLang, getText, src, first, lastRow, deadlineMs, resume); }
      catch (e) {
        store.drop(buf);
        return answer({ err: 'failed', msg: String(e && e.message ? e.message : e) });
      }
    },

    // Drop `coreLang`'s compiled queries after a `:TSInstall` (the store reloads the
    // grammar; queries compiled against the old one are stale).
    reload(coreLang) {
      const reg = store.resolveReg(coreLang) || coreLang;
      for (const [key, q] of [...queries]) {
        if (key.startsWith(reg + '\0')) { if (q.delete) q.delete(); queries.delete(key); }
      }
    },
  };
}
