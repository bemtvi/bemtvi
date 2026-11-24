// In-worker tree-sitter INDENTATION for the bemtvi edit-host.
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
// bemtvi-ts's `engine.rs::indent` (itself a port of nvim-treesitter's `indent.lua`), so
// the browser indents identically to native. Indent queries come from nvim-treesitter
// (bundled offline by gen-treesitter, or cached in OPFS by `:TSInstall`) — the grammar
// npm packages don't ship a usable one.
//
// Coordinates: web-tree-sitter reports rows/columns in UTF-16 code units (JS string
// indices). The leading whitespace and code that drive indentation are ASCII, so a
// column equals a byte there; the algorithm stays in JS string units throughout.

import { createScmLoader, NEED_TEXT } from './ts-buffers.js';

// One indents.scm capture's `#set!` directives the algorithm consults.
function beginMeta(props) {
  return {
    // `(#set! indent.immediate)` — indent even for a node that opens+closes on one line.
    immediate: props && 'indent.immediate' in props,
    // `(#set! indent.start_at_same_line)` — indent even when the node starts on this line.
    startAtSameLine: props && 'indent.start_at_same_line' in props,
  };
}

// Captured node ids by indent role, built once per `indent()`. Mirrors engine.rs's
// IndentMaps and its scoping: the algorithm only consults captured nodes that start at
// or above the target line (the decision node and its ancestors), so the capture pass
// covers the rows from the top through `line` rather than the whole tree; `lineCount`
// is the real line count (a `line` at or past it covers everything, which is correct
// there: every consulted node is above the phantom line).
function buildIndentMaps(query, root, line, lineCount) {
  const maps = {
    begin: new Map(), end: new Set(), dedent: new Set(), branch: new Set(),
    ignore: new Set(), align: new Set(), auto: new Set(), zero: new Set(),
  };
  const opts = line + 1 < lineCount
    ? { startPosition: { row: 0, column: 0 }, endPosition: { row: line + 1, column: 0 } }
    : {};
  for (const m of query.matches(root, opts)) {
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

// Build the worker's indenter over the shared buffer store (ts-buffers.js): the
// buffer's text (as lines) and tree come from the store, kept current from the core's
// edit deltas, so an indent ask (`o`, `<CR>`, `=`) neither copies nor re-parses the
// buffer. The Worker installs the returned methods on globalThis for the FFI bridge and
// warms grammars via `ensureForFrame` each redraw.
export function createIndenter(store) {
  const scm = createScmLoader(store, 'indents', 'indenter');
  return {
    pendingLoads: () => scm.pendingLoads(),
    ensureForFrame: (frame) => scm.ensureForFrame(frame),
    // Whether ts-indent is available for `coreLang` (a grammar with an indents.scm is
    // loaded) — the core reads this to tell an inconclusive query (fall back to
    // copy-previous) from no-ts-indent (column 0). Kicks off a load on first miss.
    available: (coreLang) => scm.available(coreLang),

    // Target indent width in columns for `line` of buffer `buf` (`coreLang`, at the
    // core's text `version`), `NEED_TEXT` when the store needs the text (`getText()` gave
    // none), or -1 for the honest fallback cases (grammar still loading, no
    // grammar/indents, an inconclusive query).
    // `sw` is the resolved shiftwidth. Synchronous.
    indent(buf, version, coreLang, getText, line, sw /*, ts */) {
      const reg = store.resolveReg(coreLang);
      if (!reg) return -1;
      const e = scm.entry(reg);
      if (!e) return -1;
      const b = store.buffer(buf, version, reg, e.language, getText);
      if (b === NEED_TEXT) return NEED_TEXT;
      if (b.failed) return -1;
      if (store.parse(buf, b, { complete: true }) !== 'done') return -1;
      try {
        const lineCount = Math.max(0, b.lines.length - 1);
        const maps = buildIndentMaps(e.query, b.tree.rootNode, line, lineCount);
        const r = computeIndent(maps, b.tree.rootNode, b.lines, line, sw);
        return r == null ? -1 : r;
      } catch (err) {
        console.error('[bemtvi] indenter: indent failed for', reg, err);
        return -1;
      }
    },

    // Evict `coreLang`'s indents.scm after a `:TSInstall` so the next ask reloads it.
    reload: (coreLang) => scm.reload(coreLang),
  };
}
