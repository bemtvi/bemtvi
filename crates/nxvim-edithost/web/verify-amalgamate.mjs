// Node-only regression for amalgamate-plugins.mjs — no wasm/browser needed. Guards the
// per-plugin IDENTITY the browser bundle depends on: a bundled plugin that calls
// nx.shada.plugin() / nx.view.create{ persist } at load (e.g. nxvim-tree's session
// persistence) must attribute to its own namespace. The wasm boot sources the whole bundle
// under one chunk name (`@init.lua`) with an empty runtimepath, so an inline
// `preload = function() … end` inherits `@init.lua` and attributes to NOTHING — the crash
// this asserts against ("nx.shada.plugin: this caller attributes to no plugin"). The
// amalgamator must instead (a) register a synthetic rtp root per plugin and (b) compile each
// module through `load(<src>, "@<root>/lua/<rel>")` so it carries a plugin-scoped source.
//
// The semantic proof that this shape attributes correctly in a real Lua VM lives in the
// server harness (shada.rs → browser_bundled_plugin_attributes_via_its_named_chunk); this
// guards the amalgamator OUTPUT so a regression is caught without a wasm build. Run:
//   node verify-amalgamate.mjs
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { amalgamate } from "./amalgamate-plugins.mjs";

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    failures++;
    if (detail) console.log(`      ${detail}`);
  }
}

// A fixture plugin whose single module (a) calls nx.shada.plugin() at load and (b) contains
// its OWN nested long strings (`]]`, `]=]`, `]==]`) so the long-bracket encoder must pick a
// safe level and round-trip the body byte-for-byte.
const root = mkdtempSync(join(tmpdir(), "amalg-"));
const modDir = join(root, "treeish", "lua", "treeish");
mkdirSync(modDir, { recursive: true });
const body = [
  "local M = {}",
  "local store = nx.shada.plugin()",
  "-- nested long strings that would break a naive wrapper:",
  "M.a = [[ plain ]]",
  "M.b = [=[ level one ]=]",
  "M.c = [==[ level two ]==]",
  "return M",
].join("\n");
writeFileSync(join(modDir, "init.lua"), body);

const out = amalgamate([join(root, "treeish")]);

check(
  "registers a synthetic rtp root for the plugin",
  out.includes('nx._add_rtp("/nxvim-plugins/treeish")'),
  "expected an nx._add_rtp(...) line so the plugin's chunks attribute to a namespace",
);
check(
  "compiles the module through load() with a plugin-scoped chunk name",
  out.includes('package.preload["treeish"] = assert(load(') &&
    out.includes('"@/nxvim-plugins/treeish/lua/treeish/init.lua"'),
  "expected load(<src>, \"@/nxvim-plugins/treeish/lua/treeish/init.lua\")",
);
check(
  "does NOT use the old inline function form (inherits @init.lua → no attribution)",
  !out.includes('package.preload["treeish"] = function('),
  "an inline preload function inherits the bundle's @init.lua source and can't attribute",
);
check(
  "round-trips a body containing its own nested long strings",
  out.includes("M.c = [==[ level two ]==]") && out.includes("M.b = [=[ level one ]=]"),
  "the long-bracket encoder must pick a level above the body's own ]==] run",
);
check(
  "the load() long-string level clears the body's nested brackets",
  // The body contains a `]==]` run (level 2), so the wrapper must be level >= 3 (`[===[`).
  out.includes("assert(load([===["),
  "expected the wrapping long bracket to be [===[ or deeper",
);

console.log(failures ? `\n${failures} check(s) failed` : "\nall checks passed");
process.exit(failures ? 1 : 0);
