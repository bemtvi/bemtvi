// The edit-host worker's shared tree-sitter BUFFER STORE: one copy of each buffer's text,
// one parser and one tree per buffer, shared by every in-tick tree-sitter runner —
// queries (ts-query.js), indentation (ts-indent.js), folds (ts-folds.js) and text
// objects (ts-textobjects.js). It also owns the web-tree-sitter runtime and the grammars
// (one `Language` per grammar, so every runner's queries compile against the language
// the shared trees were parsed with).
//
// Nothing here may cost in proportion to the buffer per keystroke (the editor must
// never freeze):
//
// - Each buffer's text is kept as LINES, with a Fenwick tree of their lengths (UTF-16
//   code units, newline included) for row ↔ index in O(log n). The core forwards every
//   edit (`edit`, from WasmSyntax::edit through `eh_js_ts_edit`) when it next syncs its
//   engine: only the touched lines change, the edit's byte columns are converted to
//   UTF-16 within those lines alone, and the tree is edited for an incremental re-parse.
//   The whole text is read only when the store's copy isn't at the core's version — the
//   first ask about a buffer, after a re-open (undo, reload: natively a full re-parse
//   too), or after an edit it couldn't apply. The runners then answer `NEED_TEXT`, and
//   the core asks again with the text.
// - The parser reads straight from the lines (its input callback gets a UTF-16 index).
// - A parse runs in slices of PARSE_SLICE_MS, one background CHAIN per buffer. An ask
//   that may advance it runs a slice first; one that finds it unfinished either falls
//   run it to the end within the call (indent, folds, text objects, which must answer
//   now) or is told "not yet" (a query) and is woken when it finishes (`owe`). Edits that arrive while a started parse is reading the lines
//   are queued and applied when it finishes, as one more incremental parse.
//
// Coordinates: web-tree-sitter works in UTF-16 code units (indices and columns); the
// core in UTF-8 bytes. Conversions happen within single lines only.

import { EXT, FT, REGISTRY } from './grammars.js';

const V = new URL('./vendor/', import.meta.url);
// Where `:TSInstall` caches grammars in OPFS: /.bemtvi/treesitter/<lang>/parser.wasm.
export const TS_DIR = ['.bemtvi', 'treesitter'];

// How long one slice of a parse may run (the native engine's PARSE_DEADLINE).
export const PARSE_SLICE_MS = 50;
// The longest chunk handed to the parser per read: web-tree-sitter copies it into a
// 10 KiB buffer (5120 UTF-16 code units).
const CHUNK_MAX = 4096;

// The answer a runner gives when the store's copy isn't at the core's version and the
// core didn't pass the text: the core asks again with it.
export const NEED_TEXT = '{"need":"text"}';

export async function opfsReadBytes(parts) {
  let dir = await navigator.storage.getDirectory();
  for (const p of parts.slice(0, -1)) dir = await dir.getDirectoryHandle(p, { create: false });
  const fh = await dir.getFileHandle(parts[parts.length - 1], { create: false });
  return new Uint8Array(await (await fh.getFile()).arrayBuffer());
}

export async function opfsReadText(parts) {
  return new TextDecoder().decode(await opfsReadBytes(parts));
}

export async function fetchText(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${r.status} fetching ${url}`);
  return r.text();
}

// A Fenwick (binary indexed) tree over line lengths: prefix sums and point updates in
// O(log n), the row holding an index in O(log n), rebuilt in O(n) only when the number
// of lines changes.
class Lengths {
  constructor(lines) { this.build(lines); }
  // From the lines themselves (each counts its newline, except the last): one pass.
  build(lines) {
    const n = lines.length;
    const t = new Float64Array(n + 1);
    for (let i = 0; i < n; i++) t[i + 1] = lines[i].length + (i < n - 1 ? 1 : 0);
    for (let i = 1; i <= n; i++) {
      const j = i + (i & -i);
      if (j <= n) t[j] += t[i];
    }
    this.n = n;
    this.t = t;
    this.high = 1;
    while (this.high * 2 <= n) this.high *= 2;
  }
  // Sum of the first `r` lengths: the index where row `r` starts.
  prefix(r) {
    let s = 0;
    for (let i = r; i > 0; i -= i & -i) s += this.t[i];
    return s;
  }
  add(r, delta) {
    for (let i = r + 1; i <= this.n; i += i & -i) this.t[i] += delta;
  }
  // The row containing `index`, and the index's column in it.
  find(index) {
    let pos = 0, rem = index;
    for (let k = this.high; k > 0; k >>= 1) {
      if (pos + k <= this.n && this.t[pos + k] <= rem) { pos += k; rem -= this.t[pos]; }
    }
    return [pos, rem];
  }
  total() { return this.prefix(this.n); }
}

// The UTF-16 column of byte column `byteCol` in `line`. Throws when it falls inside a
// character.
export function u16Col(line, byteCol) {
  let b = 0, i = 0;
  while (i < line.length && b < byteCol) {
    const c = line.charCodeAt(i);
    if (c < 0x80) b += 1;
    else if (c < 0x800) b += 2;
    else if ((c & 0xfc00) === 0xd800 && i + 1 < line.length) { b += 4; i++; }
    else b += 3;
    i++;
  }
  if (b !== byteCol) throw new Error(`byte column ${byteCol} is not a character boundary`);
  return i;
}

// Byte columns for UTF-16 columns of each row: `cols` maps row → Set of UTF-16 columns,
// the result maps row → Map(UTF-16 column → byte column). One sorted sweep per row, over
// that row's text only. `late()` is polled in the loop; when it says so, returns null.
export function byteColumns(b, cols, late = () => false) {
  const bytes = new Map();
  for (const [row, set] of cols) {
    const line = b.lines[row] || '';
    const sorted = [...set].sort((x, y) => x - y);
    const perCol = new Map();
    let byte = 0, i = 0;
    for (const target of sorted) {
      while (i < target && i < line.length) {
        const c = line.charCodeAt(i);
        if (c < 0x80) byte += 1;
        else if (c < 0x800) byte += 2;
        else if ((c & 0xfc00) === 0xd800 && i + 1 < line.length) { byte += 4; i++; }
        else byte += 3;
        i++;
        if (late()) return null;
      }
      perCol.set(target, byte);
    }
    bytes.set(row, perCol);
  }
  return bytes;
}

// Build the store. `onReady()` is called when something an ask was told "not yet" about
// has settled — a grammar finished loading (or failed), or a parse a query waited on
// finished — so the worker can tell the core to ask again. Always asynchronously.
export function createBufferStore({ onReady } = {}) {
  const bundled = new Set();   // grammars vendored in ./vendor/
  const installed = new Set(); // grammars cached in OPFS by `:TSInstall`
  const langs = new Map();     // reg → { language } | { failed: msg } | null (none)
  const loadGen = new Map();   // reg → generation, bumped by reload (drops stale loads)
  const loading = new Map();   // reg → the in-flight load's promise
  // buffer id → state (see `load`), or a `{ failed, version, reg }` tombstone
  const buffers = new Map();
  let initError = null;        // the runtime failed to start
  let notifyArmed = false;
  let api = null;              // { Parser, Language, Query } once the runtime is up

  // Tell the worker something settled. Deferred to its own task, so it can never run
  // inside a call from the core (that would re-enter the wasm host mid-call), and
  // coalesced, so a burst of settles costs one re-run.
  function notify() {
    if (notifyArmed || !onReady) return;
    notifyArmed = true;
    setTimeout(() => { notifyArmed = false; onReady(); }, 0);
  }

  let ready = false;
  let readyResolve;
  const readyP = new Promise((r) => { readyResolve = r; });
  let askedBeforeReady = false;

  (async () => {
    const m = await import('./vendor/web-tree-sitter/web-tree-sitter.js');
    await m.Parser.init({ locateFile: (f) => new URL('web-tree-sitter/' + f, V).href });
    api = { Parser: m.Parser, Language: m.Language, Query: m.Query };
    try {
      const res = await fetch(new URL('manifest.json', V));
      if (res.ok) for (const l of (await res.json()).languages || []) bundled.add(l);
    } catch (e) { console.error('[bemtvi] ts-buffers: vendor manifest load failed:', e); }
    await refreshInstalled();
  })()
    .catch((e) => {
      initError = 'the tree-sitter runtime failed to start: ' + (e && e.message ? e.message : e);
      console.error('[bemtvi] ts-buffers: init failed:', e && e.stack ? e.stack : e);
    })
    .finally(() => {
      ready = true;
      readyResolve();
      if (askedBeforeReady) notify();
    });

  // The latest refresh of the installed list; a load waits for it, so one that follows
  // a `:TSInstall` reload sees the new grammar instead of falling back to the bundle.
  let installedP = Promise.resolve();
  async function refreshInstalled() {
    try {
      for (const l of JSON.parse(await opfsReadText([...TS_DIR, 'manifest.json']))) installed.add(l);
    } catch { /* nothing installed yet */ }
  }

  function resolveReg(coreLang) {
    if (!coreLang) return null;
    const reg = FT[coreLang] || coreLang;
    return REGISTRY[reg] ? reg : null;
  }

  // Load `reg`'s grammar (idempotent). An installed copy wins over the bundle. A
  // `reload` while this is in flight bumps the generation, and the stale result is
  // dropped rather than overwriting the slot the reload cleared.
  // Every caller gets the same in-flight promise, so one that asks while another's load
  // is running waits for it rather than seeing "nothing yet" as "no grammar".
  function ensure(reg) {
    if (langs.has(reg)) return Promise.resolve();
    if (loading.has(reg)) return loading.get(reg);
    const p = load_(reg);
    loading.set(reg, p);
    return p;
  }
  async function load_(reg) {
    const gen = loadGen.get(reg) || 0;
    let slot;
    try {
      if (!ready) await readyP;
      await installedP;
      if (initError) slot = { failed: initError };
      else if (installed.has(reg)) slot = { language: await api.Language.load(await opfsReadBytes([...TS_DIR, reg, 'parser.wasm'])) };
      else if (bundled.has(reg)) slot = { language: await api.Language.load(new URL('grammars/' + reg + '.wasm', V).href) };
      else slot = null;
    } catch (e) {
      console.error('[bemtvi] ts-buffers: failed to load', reg, e);
      slot = { failed: String(e && e.message ? e.message : e) };
    } finally {
      loading.delete(reg);
      if ((loadGen.get(reg) || 0) === gen) langs.set(reg, slot === undefined ? null : slot);
      notify();
    }
  }

  // --- the buffer model ---------------------------------------------------------

  const lineLen = (b, i) => b.lines[i].length + (i < b.lines.length - 1 ? 1 : 0);

  // A fresh state for `text` at `version` (the text ends in a newline: the rope's
  // phantom last line becomes a final empty entry).
  function load(reg, language, version, text) {
    const parser = new api.Parser();
    parser.setLanguage(language);
    const b = {
      reg, version, lines: text.split('\n'), lens: null, parser,
      tree: null, oldTree: null,
      parsing: true, started: false, chain: false,
      pending: [], owed: false,
    };
    b.lens = new Lengths(b.lines);
    // The parser's input: the text from UTF-16 `index` on, read from the lines.
    b.input = (index) => {
      if (index >= b.lens.total()) return '';
      let [row, col] = b.lens.find(index);
      const last = b.lines.length - 1;
      let s = b.lines[row].slice(col) + (row < last ? '\n' : '');
      while (s.length < CHUNK_MAX / 2 && row < last) {
        row++;
        s += b.lines[row] + (row < last ? '\n' : '');
      }
      if (s.length > CHUNK_MAX) {
        s = s.slice(0, CHUNK_MAX);
        if ((s.charCodeAt(CHUNK_MAX - 1) & 0xfc00) === 0xd800) s = s.slice(0, CHUNK_MAX - 1);
      }
      return s;
    };
    return b;
  }

  function dropBuffer(buf) {
    const b = buffers.get(buf);
    if (!b) return;
    // An ask was told "not yet" about this buffer: re-asked, it gets a real answer (or
    // "invalid buffer"), instead of waiting on a parse that will never finish.
    if (b.owed) notify();
    buffers.delete(buf);
    if (b.failed) return;
    if (b.tree) b.tree.delete();
    if (b.oldTree) b.oldTree.delete();
    b.parser.delete();
  }

  // Apply one edit to `b`'s lines (and to its tree, when it has one): the range from
  // (sRow, sByte) to (oRow, oByte), byte columns, replaced by `text`. Throws on an edit
  // that doesn't fit the lines (the caller drops the state and re-reads the text).
  function applyEdit(b, e) {
    const { sRow, sByte, oRow, oByte, text } = e;
    if (sRow < 0 || oRow < sRow || oRow >= b.lines.length) throw new Error('edit out of range');
    const sLine = b.lines[sRow], oLine = b.lines[oRow];
    const sCol = u16Col(sLine, sByte), oCol = u16Col(oLine, oByte);
    const startIndex = b.lens.prefix(sRow) + sCol;
    const oldEndIndex = b.lens.prefix(oRow) + oCol;
    const parts = text.split('\n');
    const head = sLine.slice(0, sCol), tail = oLine.slice(oCol);
    const repl = parts.length === 1
      ? [head + text + tail]
      : [head + parts[0], ...parts.slice(1, -1), parts[parts.length - 1] + tail];
    const removed = oRow - sRow + 1;
    if (repl.length === removed) {
      for (let k = 0; k < removed; k++) {
        const before = lineLen(b, sRow + k);
        b.lines[sRow + k] = repl[k];
        b.lens.add(sRow + k, lineLen(b, sRow + k) - before);
      }
    } else {
      // The number of lines changed: splice them and rebuild the lengths (O(lines), a
      // numeric pass; only when a line is added or removed).
      if (repl.length < 10000) b.lines.splice(sRow, removed, ...repl);
      else b.lines = b.lines.slice(0, sRow).concat(repl, b.lines.slice(oRow + 1));
      b.lens.build(b.lines);
    }
    const base = b.tree || b.oldTree;
    if (base) {
      base.edit({
        startIndex,
        oldEndIndex,
        newEndIndex: startIndex + text.length,
        startPosition: { row: sRow, column: sCol },
        oldEndPosition: { row: oRow, column: oCol },
        newEndPosition: {
          row: sRow + parts.length - 1,
          column: parts.length === 1 ? sCol + text.length : parts[parts.length - 1].length,
        },
      });
      b.oldTree = base;
      b.tree = null;
    }
    b.parsing = true;
    b.started = false;
  }

  // One slice of `b`'s parse. True once the tree is current. Edits that arrived while
  // it was reading the lines are applied when it finishes, and parsing goes on.
  function parseSlice(b, sliceMs) {
    b.started = true;
    const until = performance.now() + sliceMs;
    const opts = sliceMs === Infinity ? {} : { progressCallback: () => performance.now() > until };
    const tree = b.parser.parse(b.input, b.oldTree, opts);
    if (!tree) return false; // cancelled: the next call resumes it
    if (b.oldTree) b.oldTree.delete();
    b.oldTree = null;
    b.tree = tree;
    b.parsing = false;
    b.started = false;
    if (b.pending.length) {
      const queued = b.pending;
      b.pending = [];
      for (const e of queued) applyEdit(b, e);
      return false;
    }
    return true;
  }

  // The parse finished: if an ask was told "not yet" meanwhile, say so.
  function finished(b) {
    if (b.owed) { b.owed = false; notify(); }
  }

  // The grammar threw while parsing `b`. Keep a tombstone at this version, so asks get a
  // terminal failure instead of starting the parse over (a decor provider re-dispatched
  // by the failure would otherwise loop); a new version tries again.
  function fail(buf, b, e) {
    const msg = 'parse failed: ' + (e && e.message ? e.message : e);
    const owed = b.owed;
    if (b.tree) b.tree.delete();
    if (b.oldTree) b.oldTree.delete();
    b.parser.delete();
    const tomb = { failed: msg, version: b.version, reg: b.reg };
    buffers.set(Number(buf), tomb);
    if (owed) notify();
    return tomb;
  }

  // Carry an unfinished parse on in the background, one slice per task, one chain per
  // buffer. Ends when the parse does (whoever finished it) or the buffer is dropped.
  function ensureChain(buf, b) {
    if (b.chain) return;
    b.chain = true;
    const step = () => {
      if (buffers.get(buf) !== b) return; // dropped (or replaced)
      if (!b.parsing) { b.chain = false; finished(b); return; }
      try {
        if (parseSlice(b, PARSE_SLICE_MS)) { b.chain = false; finished(b); return; }
      } catch (e) {
        console.error('[bemtvi] ts-buffers: parse failed:', e);
        b.chain = false;
        fail(buf, b, e);
        return;
      }
      setTimeout(step, 0);
    };
    setTimeout(step, 0);
  }

  return {
    get ready() { return ready; },
    get api() { return api; },
    get initError() { return initError; },
    resolveReg,
    isInstalled: (reg) => installed.has(reg),
    isBundled: (reg) => bundled.has(reg),
    // Settles once the runtime has started (or failed to).
    whenReady: () => readyP,

    // `reg`'s grammar: `{ language }`, `{ failed: msg }`, `null` (none installed), or
    // `undefined` while it loads (the load is started; the store reports when it lands).
    language(reg) {
      if (!ready) { askedBeforeReady = true; return undefined; }
      if (initError) return { failed: initError };
      const slot = langs.get(reg);
      if (slot === undefined) ensure(reg);
      return slot;
    },
    // Settles with `reg`'s grammar slot once it has loaded (for a runner loading its own
    // query file against the shared language).
    async languageAsync(reg) {
      if (!ready) await readyP;
      if (initError) return { failed: initError };
      if (!langs.has(reg)) await ensure(reg);
      return langs.get(reg) ?? null;
    },
    warm(reg) { if (ready && !initError && !langs.has(reg)) ensure(reg); },

    // `buf`'s state at the core's text `version` in grammar `reg` (whose slot is
    // `{ language }`): the current one, or a fresh one from `getText()`. Returns a
    // tombstone (`{ failed }`) for a buffer whose parse failed at this version, and
    // `NEED_TEXT` when the text is needed and `getText()` gave none.
    buffer(buf, version, reg, language, getText) {
      let b = buffers.get(buf);
      if (b && (b.reg !== reg || b.version !== version)) { dropBuffer(buf); b = null; }
      if (b) return b;
      const text = getText();
      if (text === null || text === undefined) return NEED_TEXT;
      b = load(reg, language, version, text);
      buffers.set(buf, b);
      return b;
    },

    // Bring `b`'s tree up to its text. With `complete`, the parse runs to the end within
    // the call (the in-tick asks that must answer now: indent, folds, text objects — they
    // always parsed within the call; now they parse only after an edit, incrementally,
    // and share the tree). Otherwise, with `sliceMs > 0`, an unfinished parse runs one
    // slice that long first. Returns 'done' when the tree is current, 'parsing' when it
    // isn't (a background chain carries the parse on), or a tombstone when the grammar
    // threw. `owe` asks to be woken (`onReady`) when the parse finishes.
    parse(buf, b, { sliceMs = 0, owe = false, complete = false } = {}) {
      if (!b.parsing) return 'done';
      if (complete) {
        try {
          // `parseSlice` comes back false after applying edits that arrived mid-parse
          // (another parse is then due), so go round until the tree is current.
          while (b.parsing && !parseSlice(b, Infinity));
        } catch (e) {
          return fail(buf, b, e);
        }
        finished(b);
        return 'done';
      }
      if (sliceMs > 0) {
        let done;
        try { done = parseSlice(b, sliceMs); }
        catch (e) { return fail(buf, b, e); }
        if (done) { finished(b); return 'done'; }
      }
      if (owe) b.owed = true;
      ensureChain(buf, b);
      return 'parsing';
    },

    // Something keeps the worker busy: grammar loads, the runtime starting, background
    // parse chains, or a wake waiting on its timer (the worker must not block in
    // `Atomics.wait` before that timer has run, or the wake it carries is lost).
    pendingLoads() {
      let parsing = 0;
      for (const b of buffers.values()) if (b.chain) parsing++;
      return loading.size + (ready ? 0 : 1) + parsing + (notifyArmed ? 1 : 0);
    },

    // An edit to buffer `buf` (text version `prev` → `next`): the range from
    // (sRow, sByte) to (oRow, oByte), byte columns before the edit, replaced by `text`.
    // Applied to the store's copy now, or queued while a started parse is reading it. A
    // copy that isn't at `prev` (or can't take the edit) is dropped and re-read on the
    // next ask.
    edit(buf, prev, next, sRow, sByte, oRow, oByte, text) {
      const b = buffers.get(buf);
      if (!b) return;
      if (b.failed || b.version !== prev) { dropBuffer(buf); return; }
      b.version = next;
      const e = { sRow, sByte, oRow, oByte, text };
      if (b.started) { b.pending.push(e); return; }
      try { applyEdit(b, e); }
      catch (err) {
        console.error('[bemtvi] ts-buffers: edit not applied, re-reading the buffer:', err);
        dropBuffer(buf);
      }
    },

    forget(buf) { dropBuffer(buf); },
    drop(buf) { dropBuffer(buf); },

    // Evict `reg`'s grammar and its buffers after a `:TSInstall`, so the next ask
    // reloads it.
    reload(coreLang) {
      const reg = resolveReg(coreLang) || (coreLang && (FT[coreLang] || coreLang));
      if (!reg) return null;
      loadGen.set(reg, (loadGen.get(reg) || 0) + 1);
      langs.delete(reg);
      for (const [buf, b] of [...buffers]) if (b.reg === reg) dropBuffer(buf);
      installedP = refreshInstalled();
      return reg;
    },
  };
}

// The query-file half of an in-tick runner (indent, folds, text objects): each loads one
// `.scm` per grammar — `kind` (`"indents"`, `"folds"`, `"textobjects"`) — and compiles it
// against the store's shared language for that grammar, so its queries run on the
// store's trees. An OPFS copy (from `:TSInstall`) wins over the bundled one;
// `<kind>.json` in the vendor dir lists the grammars that bundle one. `entry(reg)` is
// `{ language, query }` once loaded, `null` for any honest "none here" (no grammar, no
// such file, a load or compile failure: the caller falls back), `undefined` while it
// loads (the load is started).
export function createScmLoader(store, kind, label) {
  const bundledScm = new Set();
  const entries = new Map();
  const loading = new Set();
  let listed = false;
  const listP = (async () => {
    try {
      const res = await fetch(new URL(kind + '.json', V));
      if (res.ok) for (const l of await res.json()) bundledScm.add(l);
    } catch { /* none bundled */ }
    listed = true;
  })();

  async function ensure(reg) {
    if (entries.has(reg) || loading.has(reg)) return;
    loading.add(reg);
    try {
      await store.whenReady();
      await listP;
      const slot = await store.languageAsync(reg);
      if (!slot || slot.failed) { entries.set(reg, null); return; }
      let scm = null;
      try { scm = await opfsReadText([...TS_DIR, reg, kind + '.scm']); }
      catch {
        if (!store.isInstalled(reg) && bundledScm.has(reg)) {
          scm = await fetchText(new URL(kind + '/' + reg + '.scm', V).href);
        }
      }
      if (!scm || !scm.trim()) { entries.set(reg, null); return; }
      entries.set(reg, { language: slot.language, query: new store.api.Query(slot.language, scm) });
    } catch (e) {
      console.error(`[bemtvi] ${label}: failed to load`, reg, e);
      entries.set(reg, null);
    } finally {
      loading.delete(reg);
    }
  }

  return {
    entry(reg) {
      const e = entries.get(reg);
      if (e === undefined) ensure(reg);
      return e;
    },
    pendingLoads() { return loading.size + (listed ? 0 : 1); },
    // Warm the grammars for the languages on screen, so answers are ready before use.
    ensureForFrame(frame) {
      if (!frame || !Array.isArray(frame.windows)) return;
      for (const w of frame.windows) {
        let reg = store.resolveReg(w.filetype);
        if (!reg && w.file_name && !w.unnamed) {
          const base = String(w.file_name).split(/[\\/]/).pop().toLowerCase();
          const dot = base.lastIndexOf('.');
          const ext = dot >= 0 ? base.slice(dot + 1) : '';
          reg = REGISTRY[EXT[ext]] ? EXT[ext] : null;
        }
        if (reg) ensure(reg);
      }
    },
    // Whether this runner can answer for `coreLang` (its `.scm` is loaded). Kicks off a
    // load on first miss.
    available(coreLang) {
      const reg = store.resolveReg(coreLang);
      if (!reg) return false;
      return !!this.entry(reg);
    },
    // Evict `coreLang`'s query after a `:TSInstall`, so the next ask reloads it.
    reload(coreLang) {
      const reg = store.resolveReg(coreLang) || (coreLang && (FT[coreLang] || coreLang));
      if (!reg) return;
      const e = entries.get(reg);
      if (e && e.query) e.query.delete();
      entries.delete(reg);
    },
  };
}
