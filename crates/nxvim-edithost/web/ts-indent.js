// In-worker tree-sitter INDENTATION for the nxvim edit-host.
//
// Unlike highlighting — a pure UI-thread overlay that can repaint a frame late
// (highlight.js) — indentation has to be decided *synchronously, inside the editor
// tick*: when `o` / `O` / `<CR>` / `=` runs, the core asks "what column should this
// line start at?" mid-keystroke and edits the buffer with the answer. The tick runs in
// THIS Web Worker, so the answer must come from here, synchronously. web-tree-sitter's
// parse + query are synchronous once a grammar is loaded, so this module loads grammars
// in the worker (async, ahead of time) and then answers `indent()` synchronously, which
// the Rust core reaches through the `eh_js_ts_*` FFI bridge (web/eh-lib.js).
//
// The indent rules are nvim-treesitter's `indents.scm` (the `@indent.begin` /
// `@indent.end` / … capture vocabulary), and the algorithm below is a faithful port of
// nxvim-ts's `engine.rs::indent` (itself a port of nvim-treesitter's `indent.lua`), so
// the browser indents identically to native. Indent queries come from nvim-treesitter
// (bundled offline by gen-treesitter, or cached in OPFS by `:TSInstall`) — the grammar
// npm packages don't ship a usable one.
//
// Coordinates: web-tree-sitter reports rows/columns in UTF-16 code units (JS string
// indices). The leading whitespace and code that drive indentation are ASCII, so a
// column equals a byte there; the algorithm stays in JS string units throughout.

import { EXT, FT, REGISTRY } from './grammars.js';

// web-tree-sitter is imported *dynamically* (below, inside init) rather than statically:
// a static import that fails (e.g. the emscripten glue tripping over a worker-vs-window
// difference) would abort the whole worker module, silently hanging the editor. Loaded
// lazily and guarded, a failure instead just leaves ts-indent unavailable (the core falls
// back), and surfaces a real error in the console. Filled in by `createIndenter`'s init.
let Parser, Language, Query;

// Vendored assets live next to this module (web/vendor/), same as highlight.js.
const V = new URL('./vendor/', import.meta.url);
// OPFS path where `:TSInstall` caches grammars: /.nxvim/treesitter/<lang>/{parser.wasm,indents.scm}.
const TS_DIR = ['.nxvim', 'treesitter'];

// --- OPFS (worker thread) -----------------------------------------------------------
// The async File System Access API works in a Worker too; the grammar cache is small and
// off the hot path (loaded ahead of the keystroke), so async acquisition is fine.
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

// One indents.scm capture's `#set!` directives the algorithm consults.
function beginMeta(props) {
  return {
    // `(#set! indent.immediate)` — indent even for a node that opens+closes on one line.
    immediate: props && 'indent.immediate' in props,
    // `(#set! indent.start_at_same_line)` — indent even when the node starts on this line.
    startAtSameLine: props && 'indent.start_at_same_line' in props,
  };
}

// Captured node ids by indent role, built once per `indent()` by running indents.scm over
// the whole tree (ancestors anywhere can carry a capture). Mirrors engine.rs's IndentMaps.
function buildIndentMaps(query, root) {
  const maps = {
    begin: new Map(), end: new Set(), dedent: new Set(), branch: new Set(),
    ignore: new Set(), align: new Set(), auto: new Set(), zero: new Set(),
  };
  for (const m of query.matches(root)) {
    const props = m.setProperties;
    for (const c of m.captures) {
      const name = c.name;
      if (name.startsWith('_')) continue; // internal/predicate capture, not an indent role
      const id = c.node.id;
      switch (name) {
        case 'indent.begin': maps.begin.set(id, beginMeta(props)); break;
        case 'indent.end': maps.end.add(id); break;
        case 'indent.dedent': maps.dedent.add(id); break;
        case 'indent.branch': maps.branch.add(id); break;
        case 'indent.ignore': maps.ignore.add(id); break;
        case 'indent.align': maps.align.add(id); break;
        case 'indent.auto': maps.auto.add(id); break;
        case 'indent.zero': maps.zero.add(id); break;
        default: break;
      }
    }
  }
  return maps;
}

// The smallest node covering one column on a line — engine.rs's `node_at`
// (descendant_for_point_range(row,col .. row,col+1)).
function nodeAt(root, row, col) {
  return root.descendantForPosition({ row, column: col }, { row, column: col + 1 });
}
// Leading-whitespace length (indent depth) of `lines[row]`, in JS string units.
function leadingWs(lines, row) {
  const s = lines[row] || '';
  let i = 0;
  while (i < s.length && (s[i] === ' ' || s[i] === '\t')) i++;
  return i;
}
// 0-indexed nearest non-blank row at or above `start`, or -1 if none.
function prevNonBlank(lines, start) {
  for (let r = start; r >= 0; r--) if ((lines[r] || '').trim() !== '') return r;
  return -1;
}

// Target indent WIDTH IN COLUMNS for `line` of `lines`, or null when there's no grammar
// rule / the query is inconclusive (`@indent.auto`) — caller falls back. A faithful port
// of engine.rs::indent. `lines` is the buffer split on '\n' (the core keeps a trailing
// '\n', so the last element is the empty phantom line, never indented). `indentSize` is
// the resolved shiftwidth.
function computeIndent(maps, root, lines, line, indentSize) {
  const lineCount = Math.max(0, lines.length - 1); // exclude the trailing phantom line

  // Pick the node whose ancestry decides this line's indent: for an empty line (the
  // o/O/<CR> case) reason from the previous non-blank line's last node; otherwise from
  // this line's first node.
  const isEmpty = line >= lineCount || (lines[line] || '').trim() === '';
  let node;
  if (isEmpty) {
    const prev = prevNonBlank(lines, Math.min(line, Math.max(0, lineCount - 1)));
    if (prev < 0) return null;
    const indentcols = leadingWs(lines, prev);
    const prevTrim = (lines[prev] || '').trim();
    const col = indentcols + Math.max(0, prevTrim.length - 1);
    let n = nodeAt(root, prev, col);
    if (!n) return null;
    // A trailing comment on the previous line must not drive the indent — re-pick the
    // last node of the code preceding it.
    if (n.type.includes('comment')) {
      const first = nodeAt(root, prev, indentcols);
      if (first && first.id !== n.id) {
        const scol = n.startPosition.column;
        const cut = Math.min(Math.max(0, scol - indentcols), prevTrim.length);
        const pre = prevTrim.slice(0, cut).replace(/\s+$/, '');
        const col2 = indentcols + Math.max(0, pre.length - 1);
        const n2 = nodeAt(root, prev, col2);
        if (!n2) return null;
        n = n2;
      }
    }
    // If that last node *closes* a block (`@indent.end`), the new line sits outside it,
    // so decide from this line's own (first) node instead.
    if (maps.end.has(n.id)) {
      const nn = nodeAt(root, line, leadingWs(lines, line));
      if (!nn) return null;
      node = nn;
    } else {
      node = n;
    }
  } else {
    node = nodeAt(root, line, leadingWs(lines, line));
    if (!node) return null;
  }

  if (maps.zero.has(node.id)) return 0;

  // Accumulate indent by walking ancestors. `processed` holds start-rows already credited
  // a level, so several openers nested on one line only indent once.
  let indent = 0;
  const processed = new Set();
  let cur = node;
  while (cur) {
    const nid = cur.id;
    const srow = cur.startPosition.row;
    const erow = cur.endPosition.row;

    // `@indent.auto` (e.g. inside a raw string): defer to the editor's fallback.
    if (!maps.begin.has(nid) && !maps.align.has(nid) && maps.auto.has(nid) && srow < line && line <= erow) {
      return null;
    }
    // `@indent.ignore` block (e.g. inside a block comment): force column 0.
    if (!maps.begin.has(nid) && maps.ignore.has(nid) && srow < line && line <= erow) {
      return 0;
    }

    const rowDone = processed.has(srow);
    let isProcessed = false;

    // Branch (else/`}` opening row) and dedent close a level.
    if (!rowDone && ((maps.branch.has(nid) && srow === line) || (maps.dedent.has(nid) && srow !== line))) {
      indent -= indentSize;
      isProcessed = true;
    }

    // A node in an ERROR parent is treated as multi-line, so a half-typed opener still
    // indents (matches nvim-treesitter).
    const parent = cur.parent;
    const isInErr = !rowDone && parent && parent.hasError;

    if (!rowDone) {
      const meta = maps.begin.get(nid);
      if (meta && (srow !== erow || isInErr || meta.immediate) && (srow !== line || meta.startAtSameLine)) {
        indent += indentSize;
        isProcessed = true;
      }
    }
    // `@indent.align` (delimiter alignment) is a documented follow-up; its nodes are
    // collected above only so the auto/ignore guards stay correct.

    if (isProcessed) processed.add(srow);
    cur = cur.parent;
  }

  return Math.max(0, indent);
}

// Build the worker's indenter. Mirrors highlight.js's loader (bundled vendor / OPFS
// install caches), but loads indents.scm instead of highlights.scm and answers
// synchronously. The Worker installs the returned `indent`/`available`/`reload` on
// globalThis for the FFI bridge, and warms grammars via `ensureForFrame` each redraw.
export function createIndenter() {
  const bundled = new Set();        // languages with a grammar vendored in ./vendor/
  const bundledIndents = new Set(); // bundled languages that also vendor an indents.scm
  const installed = new Set();      // languages cached in OPFS by `:TSInstall`
  const langs = new Map();          // regName → { parser, query } once loaded, or null if unavailable
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
    } catch (e) { console.error('[nxvim] indenter: vendor manifest load failed:', e); }
    try {
      const res = await fetch(new URL('indents.json', V));
      if (res.ok) for (const l of await res.json()) bundledIndents.add(l);
    } catch { /* no bundled indents */ }
    await refreshInstalled();
    ready = true;
    readyResolve();
  })().catch((e) => console.error('[nxvim] indenter: init failed:', e && e.stack ? e.stack : e));

  async function refreshInstalled() {
    try {
      for (const l of JSON.parse(await opfsReadText([...TS_DIR, 'manifest.json']))) installed.add(l);
    } catch { /* nothing installed yet */ }
  }

  // The core's filetype → a registry grammar name (mirrors highlight.js's FT mapping), or
  // null when no grammar covers it.
  function resolveReg(coreLang) {
    if (!coreLang) return null;
    const reg = FT[coreLang] || coreLang;
    return REGISTRY[reg] ? reg : null;
  }

  // Load `reg`'s grammar + indents.scm into memory (idempotent). An installed copy wins
  // over the bundle (a `:TSInstall` of a bundled name uses the freshly fetched grammar).
  // Sets `langs[reg] = null` for any honest "no ts indent here" case (no grammar, no
  // indents.scm, or a load/compile failure) so the caller falls back rather than retries.
  async function ensure(reg) {
    if (langs.has(reg) || loading.has(reg)) return;
    loading.add(reg);
    try {
      if (!ready) await readyP;
      let parserSrc, indentScm;
      if (installed.has(reg)) {
        parserSrc = await opfsReadBytes([...TS_DIR, reg, 'parser.wasm']);
        try { indentScm = await opfsReadText([...TS_DIR, reg, 'indents.scm']); } catch { indentScm = null; }
      } else if (bundled.has(reg)) {
        parserSrc = new URL('grammars/' + reg + '.wasm', V).href;
        indentScm = bundledIndents.has(reg) ? await fetchText(new URL('indents/' + reg + '.scm', V).href) : null;
      } else {
        langs.set(reg, null);
        return;
      }
      if (!indentScm || !indentScm.trim()) { langs.set(reg, null); return; } // no indents → fall back
      const language = await Language.load(parserSrc);
      const query = new Query(language, indentScm);
      const parser = new Parser();
      parser.setLanguage(language);
      langs.set(reg, { parser, query });
    } catch (e) {
      console.error('[nxvim] indenter: failed to load', reg, e);
      langs.set(reg, null);
    } finally {
      loading.delete(reg);
    }
  }

  return {
    // How much async work is in flight: in-flight grammar loads, plus the one-time init
    // (web-tree-sitter + manifests) until ready. The worker reads this to keep its run loop
    // event-loop-live (a non-blocking `Atomics.waitAsync` park) while > 0 — a thread blocked
    // in `Atomics.wait` can't run the fetch/`Language.load` promises these depend on, so
    // without this the grammar would never finish loading between keystrokes.
    pendingLoads() {
      return loading.size + (ready ? 0 : 1);
    },

    // Warm the grammars for the languages on screen, so indentation is ready before the
    // user types. Called from the worker each redraw with the current frame.
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

    // Whether ts-indent is available for `coreLang` (a grammar with an indents.scm is
    // loaded) — the core reads this to tell an inconclusive query (fall back to
    // copy-previous) from no-ts-indent (column 0). Kicks off a load on first miss.
    available(coreLang) {
      const reg = resolveReg(coreLang);
      if (!reg) return false;
      const entry = langs.get(reg);
      if (entry === undefined) { ensure(reg); return false; }
      return !!entry;
    },

    // Target indent width in columns for `line` of `text` in `coreLang`, or -1 for the
    // honest fallback cases (grammar still loading, no grammar/indents, inconclusive
    // query). `sw` is the resolved shiftwidth. Synchronous: parse + query both are.
    indent(coreLang, text, line, sw /*, ts */) {
      const reg = resolveReg(coreLang);
      if (!reg) return -1;
      const entry = langs.get(reg);
      if (entry === undefined) { ensure(reg); return -1; } // not loaded yet — warm + fall back
      if (!entry) return -1;
      const lines = text.split('\n');
      const tree = entry.parser.parse(text);
      try {
        const maps = buildIndentMaps(entry.query, tree.rootNode);
        const r = computeIndent(maps, tree.rootNode, lines, line, sw);
        return r == null ? -1 : r;
      } catch (e) {
        console.error('[nxvim] indenter: indent failed for', reg, e);
        return -1;
      } finally {
        tree.delete();
      }
    },

    // Evict `coreLang`'s cached grammar after a `:TSInstall` so the next query reloads it
    // (picking up the freshly cached parser + indents.scm). Re-reads the OPFS install
    // manifest so a newly-installed language is known.
    reload(coreLang) {
      const reg = resolveReg(coreLang) || (coreLang && (FT[coreLang] || coreLang));
      if (reg) langs.delete(reg);
      refreshInstalled();
    },
  };
}
