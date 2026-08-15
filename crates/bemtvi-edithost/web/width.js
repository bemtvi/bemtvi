// The editor's column math, borrowed from Rust.
//
// Every renderer has to split a line into grapheme clusters and measure each one in
// screen cells, because that is the grid the server's `unicode::virtcol` puts every
// highlight span, selection and cursor column on. The TUI and the GUI link
// `unicode-segmentation` + `unicode-width` and get it exactly right for free. This page
// cannot call the edit-host — it runs in a Web Worker, and a synchronous round-trip
// would park the UI thread on a busy worker — so it loads those same two crates as
// their own ~70 KB module (`crates/bemtvi-width`, built by build.sh) and calls them
// synchronously. Before this, the page mirrored `unicode-width` in JS from generated
// tables plus a hand-written rule for the emoji ligature cases; a mirror can drift on a
// dependency bump, and linking the real crate cannot.
//
// Widths are measured PER CLUSTER and summed, which is what `virtcol` does — not
// `UnicodeWidthStr::width` of the whole line, which additionally ligatures across
// cluster boundaries (Arabic lam-alef is 1 cell to it, 2 columns to the editor).

let mem = null;   // the module's WebAssembly.Memory
let api = null;   // { reserve, segment, out_ptr }

const enc = new TextEncoder();
// Cluster → cells. Distinct clusters in a session are bounded by the alphabet on screen,
// so this converges immediately and turns the per-cell width lookups into map hits.
const widthMemo = new Map();
const MEMO_CAP = 8192;

/// Fetch + instantiate the module. Must resolve before the first paint: every renderer
/// path measures cells, and there is no fallback by design — a page that guesses widths
/// paints highlights onto the wrong glyphs, which is worse than failing visibly.
export async function initWidth(url = new URL("../dist/bemtvi_width.wasm", import.meta.url)) {
  const { instance } = await WebAssembly.instantiateStreaming(fetch(url), {}).catch(async () => {
    // `instantiateStreaming` needs an `application/wasm` content type; fall back to a
    // plain fetch so a mis-configured static host still works.
    const bytes = await (await fetch(url)).arrayBuffer();
    return WebAssembly.instantiate(bytes, {});
  });
  mem = instance.exports.memory;
  api = instance.exports;
  return { clusters, clusterWidth, segment };
}

/// True once [`initWidth`] has resolved.
export const ready = () => api !== null;

// Printable ASCII (tabs included) is one codepoint per cluster per cell, and it is most
// of most files — answer it without crossing into wasm.
const ASCII_ONLY = /^[\t\x20-\x7e]*$/;

/// Split `text` into grapheme clusters paired with their width in cells:
/// `{ parts: string[], widths: number[] }`. One wasm call per line.
export function segment(text) {
  if (!api) throw new Error("bemtvi: width module not initialised (call initWidth first)");
  if (ASCII_ONLY.test(text)) {
    const parts = [...text];
    return { parts, widths: parts.map(() => 1) };
  }
  const bytes = enc.encode(text);
  // `reserve` can grow wasm memory, which detaches every existing view — take the
  // pointer first, then build the view from a freshly read `mem.buffer`.
  const ptr = api.reserve(bytes.length);
  new Uint8Array(mem.buffer, ptr, bytes.length).set(bytes);
  const n = api.segment(bytes.length);
  // `segment` can grow memory too (the output vector), so re-read the buffer again.
  const pairs = new Uint32Array(mem.buffer, api.out_ptr(), n * 2);
  const parts = new Array(n), widths = new Array(n);
  // Walk the encoded bytes alongside the results to turn each cluster's UTF-8 length
  // into a UTF-16 slice of the original string — cheaper than decoding each cluster
  // back out of wasm memory, and it hands back real JS substrings.
  let bi = 0, ui = 0;
  for (let i = 0; i < n; i++) {
    const blen = pairs[i * 2];
    let units = 0;
    for (let k = bi; k < bi + blen; ) {
      const b = bytes[k];
      if (b < 0x80) { k += 1; units += 1; }
      else if (b < 0xe0) { k += 2; units += 1; }
      else if (b < 0xf0) { k += 3; units += 1; }
      else { k += 4; units += 2; }          // astral: a surrogate pair
    }
    parts[i] = text.slice(ui, ui + units);
    widths[i] = pairs[i * 2 + 1];
    bi += blen; ui += units;
  }
  return { parts, widths };
}

/// The grapheme clusters of `text` — the unit the column grid steps by, so a base and
/// its combining marks / variation selector / ZWJ tail stay one glyph in one cell.
export function clusters(text) {
  return segment(text).parts;
}

/// Display width of ONE grapheme cluster, in screen cells.
///
/// A cluster's width is not the sum of its codepoints': `🤴🏼` (an emoji plus its
/// skin-tone modifier) is 2 cells though each codepoint alone is 2, `❤️` (a heart plus
/// U+FE0F) is 2 though its codepoints are 1 and 0, and a ZWJ family emoji is 2 across
/// five codepoints. That is `unicode-width`'s job, and this asks it.
export function clusterWidth(cluster) {
  if (cluster.length === 1) {
    const c = cluster.charCodeAt(0);
    if (c >= 0x20 && c < 0x7f) return 1;    // printable ASCII: most cells
  }
  const hit = widthMemo.get(cluster);
  if (hit !== undefined) return hit;
  const { widths } = segment(cluster);
  // A caller can hand this a run that segments into several clusters (a `virt_text`
  // chunk); sum them, exactly as `virtcol` would.
  let w = 0;
  for (const x of widths) w += x;
  if (widthMemo.size >= MEMO_CAP) widthMemo.clear();
  widthMemo.set(cluster, w);
  return w;
}
