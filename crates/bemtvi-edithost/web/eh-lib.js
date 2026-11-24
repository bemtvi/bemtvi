// Emscripten JS library for the edit-host's *synchronous* Rust→JS calls.
//
// Almost every edit-host effect is pull-model: Rust writes a request into the `Sink`
// and the worker drains it later via an `eh_take_*` FFI export (async, off-tick). Tree-
// sitter INDENTATION can't work that way — the core needs the answer *inside* the tick,
// the moment `o`/`O`/`<CR>`/`=` runs. So these three functions are the one place the
// Rust tick calls *into* JS synchronously: emcc links them (via build.sh's
// `--js-library`) as the imports the `extern "C"` decls in lib.rs reference, and they
// forward to the worker's web-tree-sitter indenter (web/ts-indent.js), which the worker
// installs on `globalThis` after creating it.
//
// When no indenter is installed (the Node harness, or before the worker has built one),
// they degrade to "no ts indent" (-1 / 0) so the core falls back to copy-previous-line
// autoindent, then column 0 — never an error.
addToLibrary({
  // Target indent width in columns for `line` of buffer `buf` (at the core's text
  // `version`), -1 to fall back, or -2 asking for the call again with the text. The text,
  // when given, is `textLen` UTF-8 bytes at `textPtr` (0 when not given: the shared buffer
  // store keeps its own copy current from `eh_js_ts_edit` and needs the text only when its
  // copy isn't at `version`). `langPtr` is a NUL-terminated UTF-8 C string; line/sw/ts are
  // the 0-indexed line, resolved shiftwidth, and tabstop.
  eh_js_ts_indent: function (buf, version, langPtr, textPtr, textLen, line, sw, ts) {
    try {
      var f = globalThis.__bemtviTsIndent;
      if (!f) return -1;
      var getText = function () {
        return textPtr ? new TextDecoder().decode(HEAPU8.slice(textPtr, textPtr + textLen)) : null;
      };
      var r = f(buf, version >>> 0, UTF8ToString(langPtr), getText, line, sw, ts);
      if (r === '{"need":"text"}') return -2;
      return (typeof r === 'number' && r >= 0) ? (r | 0) : -1;
    } catch (e) {
      return -1;
    }
  },

  // Whether ts-indent is available for the language (a grammar with an indents.scm is
  // loaded), as 1/0 — lets the core read an inconclusive -1 from eh_js_ts_indent as
  // "fall back to copy-previous" rather than "no ts indent at all".
  eh_js_ts_available: function (langPtr) {
    try {
      var f = globalThis.__bemtviTsAvailable;
      return (f && f(UTF8ToString(langPtr))) ? 1 : 0;
    } catch (e) {
      return 0;
    }
  },

  // Drop the cached grammar for the language after a `:TSInstall`, so the next query
  // reloads it with the freshly installed parser + indents.scm / folds.scm.
  eh_js_ts_reload: function (langPtr) {
    try {
      var f = globalThis.__bemtviTsReload;
      if (f) f(UTF8ToString(langPtr));
    } catch (e) { /* best-effort */ }
  },

  // Foldable line ranges for buffer `buf` (at the core's text `version`; the text as for
  // `eh_js_ts_indent`), written into the caller's i32 out-buffer as flat
  // [start0, end0, start1, end1, …] pairs (0-based inclusive rows). Returns the total
  // number of i32s the result needs — which may EXCEED `cap`, in which case only the
  // first `cap` are written and the core re-calls with a buffer that big. `-1` means "no
  // tree-sitter folds available" (no runner / grammar still loading / no folds.scm / the
  // parse not finished), distinct from `0` (available, nothing foldable); `-2` asks for
  // the call again with the text.
  eh_js_ts_folds: function (buf, version, langPtr, textPtr, textLen, outPtr, cap) {
    try {
      var f = globalThis.__bemtviTsFolds;
      if (!f) return -1;
      var getText = function () {
        return textPtr ? new TextDecoder().decode(HEAPU8.slice(textPtr, textPtr + textLen)) : null;
      };
      var pairs = f(buf, version >>> 0, UTF8ToString(langPtr), getText);
      if (pairs === '{"need":"text"}') return -2;
      if (!pairs) return -1;
      var idx = outPtr >> 2; // i32 index into HEAP32
      var total = 0;
      for (var i = 0; i < pairs.length; i++) {
        var s0 = pairs[i][0] | 0, e0 = pairs[i][1] | 0;
        // Write only while both ints of this pair fit in the caller's buffer; keep
        // counting past it so the core knows how big to grow and re-call.
        if (total + 1 < cap) {
          HEAP32[idx + total] = s0;
          HEAP32[idx + total + 1] = e0;
        }
        total += 2;
      }
      return total;
    } catch (e) {
      return -1;
    }
  },

  // Whether tree-sitter folds are available for the language (a grammar with a folds.scm
  // is loaded), as 1/0 — lets the core tell "loaded, nothing foldable" (empty) from "no
  // grammar yet" (retry next tick).
  eh_js_ts_folds_available: function (langPtr) {
    try {
      var f = globalThis.__bemtviTsFoldsAvailable;
      return (f && f(UTF8ToString(langPtr))) ? 1 : 0;
    } catch (e) {
      return 0;
    }
  },

  // Regions of buffer `buf` (at the core's text `version`; the text as for
  // `eh_js_ts_indent`) captured by `textobjects.scm` as `capture` that contain the cursor
  // at (row, byteCol), innermost first, written into the `cap`-int `out` buffer as flat
  // `[startRow, startByteCol, endRow, endByteCol, …]` quads (the core turns them into byte
  // offsets against its rope). Returns the total ints needed (may exceed `cap`), -1 when
  // no runner / grammar / textobjects.scm / finished parse, or -2 asking for the text.
  eh_js_ts_textobjects: function (buf, version, langPtr, textPtr, textLen, capturePtr, row, byteCol, outPtr, cap) {
    try {
      var f = globalThis.__bemtviTsTextObjects;
      if (!f) return -1;
      var getText = function () {
        return textPtr ? new TextDecoder().decode(HEAPU8.slice(textPtr, textPtr + textLen)) : null;
      };
      var quads = f(buf, version >>> 0, UTF8ToString(langPtr), getText, UTF8ToString(capturePtr), row | 0, byteCol | 0);
      if (quads === '{"need":"text"}') return -2;
      if (!quads) return -1;
      var idx = outPtr >> 2; // i32 index into HEAP32
      var total = 0;
      for (var i = 0; i < quads.length; i++) {
        // Write only while all four ints of this region fit; keep counting so the core
        // can grow.
        if (total + 3 < cap) {
          for (var k = 0; k < 4; k++) HEAP32[idx + total + k] = quads[i][k] | 0;
        }
        total += 4;
      }
      return total;
    } catch (e) {
      return -1;
    }
  },

  // Run a caller's treesitter query over buffer `buf` (`btv.treesitter.query`, a
  // decor provider's `query`), restricted to rows [first, last) when `first >= 0`, with
  // `deadlineMs` to walk the tree (and, with `resume`, leave to advance an unfinished
  // parse one slice first). The text is `textLen` UTF-8 bytes at `textPtr` (not
  // NUL-terminated: a NUL in a file is just a character) and only changes when
  // `version` does, so the runner is handed a getter and decodes it only for a version
  // it hasn't seen. The runner (web/ts-query.js) answers with a JSON string, written as
  // UTF-8 into the caller's `cap`-byte `out` buffer when it fits. Returns the byte length
  // the answer needs (may exceed `cap`: the core grows its buffer and asks again, and
  // the runner returns the same answer without re-running), or -1 when no runner is
  // installed.
  eh_js_ts_query__deps: ['$stringToUTF8', '$lengthBytesUTF8'],
  eh_js_ts_query: function (buf, version, langPtr, textPtr, textLen, queryPtr, first, last, deadlineMs, resume, outPtr, cap) {
    try {
      var f = globalThis.__bemtviTsQuery;
      if (!f) return -1;
      // `slice` copies out of the (possibly shared) heap: TextDecoder won't read a view
      // over a SharedArrayBuffer.
      var lang = UTF8ToString(langPtr), src = UTF8ToString(queryPtr);
      var key = [buf, version >>> 0, lang, src, first, last].join('\u0000');
      // An answer that didn't fit last time is handed back as is: the core is asking
      // again with a bigger buffer, and running the query twice would double its cost.
      var held = globalThis.__bemtviTsQueryHeld;
      var json;
      if (held && held.key === key) {
        json = held.json;
      } else {
        // `textPtr` is 0 when the core didn't pass the text: the runner keeps its own
        // copy current from the forwarded edits and asks for the text (null here →
        // `{"need":"text"}` → -2) only when its copy isn't at `version`.
        var getText = function () {
          return textPtr ? new TextDecoder().decode(HEAPU8.slice(textPtr, textPtr + textLen)) : null;
        };
        json = f(buf, version >>> 0, lang, getText, src, first, last, deadlineMs, resume !== 0);
        if (json === '{"need":"text"}') return -2;
      }
      globalThis.__bemtviTsQueryHeld = null;
      var need = lengthBytesUTF8(json);
      if (need + 1 <= cap) stringToUTF8(json, outPtr, cap);
      // Held only for a retry the core will make: past its 64 MiB limit it gives up.
      else if (need + 1 <= 64 * 1024 * 1024) globalThis.__bemtviTsQueryHeld = { key: key, json: json };
      return need;
    } catch (e) {
      console.error('[bemtvi] ts-query bridge failed:', e);
      return -1;
    }
  },

  // An edit to buffer `buf` (version `prev` → `next`): the range from (startRow,
  // startCol) to (oldEndRow, oldEndCol), byte columns before the edit, was replaced by
  // the `textLen` UTF-8 bytes at `textPtr`. The shared buffer store applies it to its
  // copy of the text and tree, so no later ask (query, indent, folds, text objects)
  // re-reads the whole buffer.
  eh_js_ts_edit: function (buf, prev, next, startRow, startCol, oldEndRow, oldEndCol, textPtr, textLen) {
    try {
      var f = globalThis.__bemtviTsEdit;
      if (!f) return;
      var text = textLen > 0 ? new TextDecoder().decode(HEAPU8.slice(textPtr, textPtr + textLen)) : '';
      f(buf, prev >>> 0, next >>> 0, startRow, startCol, oldEndRow, oldEndCol, text);
    } catch (e) {
      console.error('[bemtvi] ts edit failed:', e);
    }
  },

  // Drop the text and tree the shared buffer store keeps for a buffer the core closed.
  eh_js_ts_forget: function (buf) {
    try {
      var f = globalThis.__bemtviTsForget;
      if (f) f(buf);
    } catch (e) { /* best-effort */ }
  },

  // Whether tree-sitter text objects are available for the language (a grammar with a
  // textobjects.scm is loaded), as 1/0.
  eh_js_ts_textobjects_available: function (langPtr) {
    try {
      var f = globalThis.__bemtviTsTextObjectsAvailable;
      return (f && f(UTF8ToString(langPtr))) ? 1 : 0;
    } catch (e) {
      return 0;
    }
  },
});
