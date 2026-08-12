// Vendor the OFFLINE tree-sitter assets the edit-host's highlighter needs into
// ./vendor/ — the BUNDLED subset of the grammar registry, for use with no network.
//
// For each bundled language this copies (1) the prebuilt grammar `.wasm` that ships
// inside its pinned npm package and (2) a *sanitized* `highlights.scm`. The runtime
// (web-tree-sitter.js + .wasm) is copied once. Everything is regenerated from the
// pinned devDependencies, so ./vendor/ is gitignored — the parent crate's build.sh
// runs this generator and copies its output into web/vendor/.
//
// The grammar list, paths, versions, and the query sanitizer all come from the shared
// registry under web/ (grammars.js, ts-sanitize.js) — the SAME modules the runtime
// `:TSInstall` path uses — so the offline set and the on-demand set can never drift.
// Languages not in BUNDLED are installed at runtime from jsDelivr instead.

import { Parser, Language, Query } from 'web-tree-sitter';
import { readFileSync, writeFileSync, mkdirSync, copyFileSync, rmSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { REGISTRY, BUNDLED, highlightSources, fetchQueryMerged } from '../../web/grammars.js';
import { sanitize } from '../../web/ts-sanitize.js';

const ROOT = dirname(dirname(fileURLToPath(import.meta.url))); // tooling dir
const NM = join(ROOT, 'node_modules');
const OUT = join(ROOT, 'vendor');
const g = (p) => join(NM, p);

async function main() {
  await Parser.init();

  rmSync(OUT, { recursive: true, force: true });
  mkdirSync(join(OUT, 'web-tree-sitter'), { recursive: true });
  mkdirSync(join(OUT, 'grammars'), { recursive: true });
  mkdirSync(join(OUT, 'queries'), { recursive: true });
  mkdirSync(join(OUT, 'indents'), { recursive: true });
  mkdirSync(join(OUT, 'folds'), { recursive: true });
  mkdirSync(join(OUT, 'textobjects'), { recursive: true });

  // Runtime (web-tree-sitter.js + .wasm) — copied once; shared by every grammar and
  // by the runtime installer.
  copyFileSync(g('web-tree-sitter/web-tree-sitter.js'), join(OUT, 'web-tree-sitter', 'web-tree-sitter.js'));
  copyFileSync(g('web-tree-sitter/web-tree-sitter.wasm'), join(OUT, 'web-tree-sitter', 'web-tree-sitter.wasm'));

  // Indent queries come from nvim-treesitter (the grammar packages don't ship a usable
  // `indents.scm`), fetched once at build time and sanitized against the grammar — the
  // offline twin of the runtime `:TSInstall` indents fetch. Best-effort: a language
  // nvim-treesitter has no indents for (or a network hiccup) is skipped, not fatal —
  // that language simply falls back to copy-previous-line autoindent in the browser.
  async function fetchText(url) {
    const r = await fetch(url);
    if (!r.ok) throw new Error(`${r.status}`);
    return r.text();
  }

  const manifest = [];
  const indented = [];
  const folded = [];
  const textobjected = [];
  for (const name of BUNDLED) {
    const cfg = REGISTRY[name];
    if (!cfg) throw new Error(`BUNDLED lists '${name}', which is not in the registry`);
    const wasmPath = g(join(cfg.pkg, cfg.wasm));
    const lang = await Language.load(wasmPath);
    const parser = new Parser();
    parser.setLanguage(lang);
    const tree = parser.parse(cfg.sample);

    const raw = highlightSources(name).map(([pkg, file]) => readFileSync(g(join(pkg, file)), 'utf8')).join('\n');
    const res = sanitize(raw, Query, lang, tree.rootNode);

    // Fail loud if the assembled query won't even compile / produces no captures — a
    // broken vendored asset should stop the build, not ship a silently-dead language.
    const caps = new Query(lang, res.text).captures(tree.rootNode).length;
    if (caps === 0) throw new Error(`${name}: sanitized query produced 0 captures on the sample — grammar/query mismatch?`);

    copyFileSync(wasmPath, join(OUT, 'grammars', `${name}.wasm`));
    writeFileSync(join(OUT, 'queries', `${name}.scm`), res.text);
    manifest.push(name);
    const dropped = res.droppedCompile + res.droppedRun;
    const note = dropped ? `  (dropped ${dropped}: ${res.droppedCompile} compile / ${res.droppedRun} run)` : '';
    console.log(`  ${name.padEnd(11)} ${res.kept}/${res.total} patterns, ${caps} sample captures${note}`);

    // Indents — best-effort, from nvim-treesitter (with its `; inherits:` chain merged,
    // so `javascript` picks up `ecma`/`jsx`), sanitized against this grammar.
    try {
      const rawIndent = await fetchQueryMerged(name, 'indents', fetchText);
      const ind = sanitize(rawIndent, Query, lang, tree.rootNode);
      // A query that compiles to zero kept patterns carries no indent rules — skip it
      // so the indenter reports "no ts indent" rather than loading a dead query.
      if (ind.kept > 0) {
        writeFileSync(join(OUT, 'indents', `${name}.scm`), ind.text);
        indented.push(name);
        console.log(`  ${''.padEnd(11)} indents: ${ind.kept}/${ind.total} patterns`);
      } else {
        console.log(`  ${''.padEnd(11)} indents: none kept — skipped`);
      }
    } catch (e) {
      console.log(`  ${''.padEnd(11)} indents: unavailable (${e.message || e}) — skipped`);
    }

    // Folds — best-effort, from nvim-treesitter (the same source native uses),
    // sanitized against this grammar. A language nvim-treesitter has no folds for is
    // skipped (it simply has no tree-sitter folds offline until `:TSInstall`).
    try {
      const rawFold = await fetchQueryMerged(name, 'folds', fetchText);
      const fld = sanitize(rawFold, Query, lang, tree.rootNode);
      if (fld.kept > 0) {
        writeFileSync(join(OUT, 'folds', `${name}.scm`), fld.text);
        folded.push(name);
        console.log(`  ${''.padEnd(11)} folds:   ${fld.kept}/${fld.total} patterns`);
      } else {
        console.log(`  ${''.padEnd(11)} folds:   none kept — skipped`);
      }
    } catch (e) {
      console.log(`  ${''.padEnd(11)} folds:   unavailable (${e.message || e}) — skipped`);
    }

    // Text objects — best-effort, from nvim-treesitter-textobjects (a SEPARATE repo),
    // sanitized against this grammar. A language that repo has no textobjects for is
    // skipped (no tree-sitter text objects offline until `:TSInstall`).
    try {
      const rawTo = await fetchQueryMerged(name, 'textobjects', fetchText);
      const to = sanitize(rawTo, Query, lang, tree.rootNode);
      if (to.kept > 0) {
        writeFileSync(join(OUT, 'textobjects', `${name}.scm`), to.text);
        textobjected.push(name);
        console.log(`  ${''.padEnd(11)} textobj: ${to.kept}/${to.total} patterns`);
      } else {
        console.log(`  ${''.padEnd(11)} textobj: none kept — skipped`);
      }
    } catch (e) {
      console.log(`  ${''.padEnd(11)} textobj: unavailable (${e.message || e}) — skipped`);
    }
  }

  writeFileSync(join(OUT, 'manifest.json'), JSON.stringify({ languages: manifest }, null, 2) + '\n');
  // The set of bundled languages that ship an indents.scm, so the worker indenter knows
  // which to load offline without probing for a 404.
  writeFileSync(join(OUT, 'indents.json'), JSON.stringify(indented, null, 2) + '\n');
  // The set of bundled languages that ship a folds.scm, the fold runner's twin of the above.
  writeFileSync(join(OUT, 'folds.json'), JSON.stringify(folded, null, 2) + '\n');
  // The set of bundled languages that ship a textobjects.scm, the text-object runner's twin.
  writeFileSync(join(OUT, 'textobjects.json'), JSON.stringify(textobjected, null, 2) + '\n');
  console.log(`vendored ${manifest.length} bundled languages (${indented.length} with indents, ${folded.length} with folds, ${textobjected.length} with textobjects) + runtime into ./vendor/`);
}

main().catch((e) => {
  console.error('gen-treesitter failed:', e);
  process.exit(1);
});
