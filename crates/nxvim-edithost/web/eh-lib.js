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
  // Target indent width in columns for `line` of the buffer text, or -1 to fall back.
  // `langPtr`/`textPtr` are NUL-terminated UTF-8 C strings; line/sw/ts are the 0-indexed
  // line, resolved shiftwidth, and tabstop.
  eh_js_ts_indent: function (langPtr, textPtr, line, sw, ts) {
    try {
      var f = globalThis.__nxvimTsIndent;
      if (!f) return -1;
      var r = f(UTF8ToString(langPtr), UTF8ToString(textPtr), line, sw, ts);
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
      var f = globalThis.__nxvimTsAvailable;
      return (f && f(UTF8ToString(langPtr))) ? 1 : 0;
    } catch (e) {
      return 0;
    }
  },

  // Drop the cached grammar for the language after a `:TSInstall`, so the next query
  // reloads it with the freshly installed parser + indents.scm / folds.scm.
  eh_js_ts_reload: function (langPtr) {
    try {
      var f = globalThis.__nxvimTsReload;
      if (f) f(UTF8ToString(langPtr));
    } catch (e) { /* best-effort */ }
  },

  // Foldable line ranges for the buffer text, written into the caller's i32 out-buffer
  // as flat [start0, end0, start1, end1, …] pairs (0-based inclusive rows). Returns the
  // total number of i32s the result needs — which may EXCEED `cap`, in which case only
  // the first `cap` are written and the core re-calls with a buffer that big. `-1` means
  // "no tree-sitter folds available" (no runner / grammar still loading / no folds.scm),
  // distinct from `0` (available, nothing foldable). `langPtr`/`textPtr` are NUL-
  // terminated UTF-8; `outPtr` is a 4-byte-aligned i32 buffer of `cap` ints.
  eh_js_ts_folds: function (langPtr, textPtr, outPtr, cap) {
    try {
      var f = globalThis.__nxvimTsFolds;
      if (!f) return -1;
      var pairs = f(UTF8ToString(langPtr), UTF8ToString(textPtr));
      if (!pairs) return -1;
      var idx = outPtr >> 2; // i32 index into HEAP32
      var total = 0;
      for (var i = 0; i < pairs.length; i++) {
        var s = pairs[i][0] | 0, e = pairs[i][1] | 0;
        // Write only while both ints of this pair fit in the caller's buffer; keep
        // counting past it so the core knows how big to grow and re-call.
        if (total + 1 < cap) {
          HEAP32[idx + total] = s;
          HEAP32[idx + total + 1] = e;
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
      var f = globalThis.__nxvimTsFoldsAvailable;
      return (f && f(UTF8ToString(langPtr))) ? 1 : 0;
    } catch (e) {
      return 0;
    }
  },

  // Byte ranges of `text`'s `textobjects.scm` nodes captured as `capture` that contain
  // byte offset `byte`, written into the `cap`-int `out` buffer as flat `[start, end, …]`
  // byte pairs, innermost first. Returns the total ints needed (may exceed `cap`), or -1
  // when no runner / grammar / textobjects.scm. The worker runner converts UTF-16↔byte.
  eh_js_ts_textobjects: function (langPtr, textPtr, capturePtr, byte, outPtr, cap) {
    try {
      var f = globalThis.__nxvimTsTextObjects;
      if (!f) return -1;
      var pairs = f(UTF8ToString(langPtr), UTF8ToString(textPtr), UTF8ToString(capturePtr), byte | 0);
      if (!pairs) return -1;
      var idx = outPtr >> 2; // i32 index into HEAP32
      var total = 0;
      for (var i = 0; i < pairs.length; i++) {
        var s = pairs[i][0] | 0, e = pairs[i][1] | 0;
        // Write only while both ints of this pair fit; keep counting so the core can grow.
        if (total + 1 < cap) {
          HEAP32[idx + total] = s;
          HEAP32[idx + total + 1] = e;
        }
        total += 2;
      }
      return total;
    } catch (e) {
      return -1;
    }
  },

  // Whether tree-sitter text objects are available for the language (a grammar with a
  // textobjects.scm is loaded), as 1/0.
  eh_js_ts_textobjects_available: function (langPtr) {
    try {
      var f = globalThis.__nxvimTsTextObjectsAvailable;
      return (f && f(UTF8ToString(langPtr))) ? 1 : 0;
    } catch (e) {
      return 0;
    }
  },
});
