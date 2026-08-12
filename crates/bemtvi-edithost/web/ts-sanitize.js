// Tree-sitter query sanitizer — shared by the build-time vendor generator
// (treesitter/scripts/gen-treesitter.mjs, under Node) and the runtime `:TSInstall`
// path (highlight.js, in the browser). Both turn a raw upstream `highlights.scm`
// into one the browser query engine accepts; keeping the logic here means the two
// paths can never drift.
//
// Why sanitize: upstream `highlights.scm` files occasionally use a predicate the
// browser engine doesn't implement, or — when a grammar and its query drift across
// versions — reference a node type the compiled grammar lacks. Either makes the
// *whole* query fail to compile, silently killing highlighting for that language. So
// we split the query into top-level patterns and keep only those that both compile
// and run against the grammar, degrading gracefully (a few patterns dropped) instead
// of cliff-edging to no highlighting.
//
// The web-tree-sitter `Query` constructor and a parsed sample's root node are passed
// in rather than imported, so this module depends on neither the Node nor the browser
// build of web-tree-sitter and works under both.

// Split a tree-sitter query into top-level patterns. A pattern is one depth-0
// S-expression (or [..] alternation, or bare node) plus its trailing @captures,
// quantifiers, anchors, and (#predicate ..) groups.
export function splitPatterns(src) {
  const pats = [];
  let i = 0;
  const n = src.length;
  const isWs = (c) => c === ' ' || c === '\t' || c === '\n' || c === '\r';
  const skipTrivia = () => {
    while (i < n) {
      if (isWs(src[i])) { i++; continue; }
      if (src[i] === ';') { while (i < n && src[i] !== '\n') i++; continue; }
      break;
    }
  };
  const consumeGroup = () => {
    let depth = 0;
    for (; i < n; i++) {
      const c = src[i];
      if (c === '"') { i++; while (i < n && src[i] !== '"') { if (src[i] === '\\') i++; i++; } continue; }
      if (c === ';') { while (i < n && src[i] !== '\n') i++; i--; continue; }
      if (c === '(' || c === '[') depth++;
      else if (c === ')' || c === ']') { depth--; if (depth === 0) { i++; return; } }
    }
  };
  const consumeBare = () => {
    if (src[i] === '"') { i++; while (i < n && src[i] !== '"') { if (src[i] === '\\') i++; i++; } i++; return; }
    while (i < n && !isWs(src[i]) && !'()[];'.includes(src[i])) i++;
  };
  const peekIsPredicate = () => {
    let j = i + 1;
    while (j < n && isWs(src[j])) j++;
    return src[j] === '#';
  };
  const consumeTrailers = () => {
    for (;;) {
      const save = i;
      skipTrivia();
      if (i >= n) return;
      const c = src[i];
      if (c === '@' || c === '.' || c === '?' || c === '*' || c === '+' || c === '!') { consumeBare(); continue; }
      if (c === '(' && peekIsPredicate()) { consumeGroup(); continue; }
      i = save;
      return;
    }
  };
  for (;;) {
    skipTrivia();
    if (i >= n) break;
    const start = i;
    if (src[i] === '(' || src[i] === '[') consumeGroup();
    else consumeBare();
    consumeTrailers();
    const text = src.slice(start, i).trim();
    if (text) pats.push(text);
  }
  return pats;
}

// Sanitize a raw, possibly-concatenated `highlights.scm` into the largest subset that
// compiles and runs against `lang`. `Query` is the web-tree-sitter Query constructor;
// `lang` is a loaded Language; `sampleRoot` is the root node of a parsed representative
// sample (so a pattern that throws only at *run* time is dropped too). Returns the
// assembled `text` plus drop counts for a loud-but-non-fatal report. `(#is?)` /
// `(#is-not?)` are the editor-only "is this a local?" predicates the browser engine
// has no table for, so they're stripped outright before splitting.
export function sanitize(rawText, Query, lang, sampleRoot) {
  const cleaned = rawText.replace(/\(#is(-not)?\?[^()]*\)/g, '');
  const pats = splitPatterns(cleaned);
  const kept = [];
  let droppedCompile = 0;
  let droppedRun = 0;
  for (const pat of pats) {
    let q;
    try { q = new Query(lang, pat); } catch { droppedCompile++; continue; }
    try { q.captures(sampleRoot); } catch { droppedRun++; continue; }
    kept.push(pat);
  }
  return {
    text: kept.join('\n\n') + '\n',
    kept: kept.length,
    total: pats.length,
    droppedCompile,
    droppedRun,
  };
}
