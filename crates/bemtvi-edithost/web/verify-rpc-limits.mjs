// Node verifier for the browser daemon wire's decode limits (rpc.mjs). msgpack lets a frame
// DECLARE a huge array / map / str / bin in a 5-byte header; with @msgpack/msgpack's defaults
// (UINT32_MAX) the reader would accept the header and buffer or allocate toward it, so one
// hostile daemon frame could take the tab down. The client must refuse such a frame at the
// header — with the same caps the native `bemtvi-rpc` reader enforces — and close the link
// loudly: pending requests reject naming the limit, `dead` resolves, the transport closes.
//
// Drives `RpcClient` over in-memory streams (no browser, no daemon). Run:
//   node verify-rpc-limits.mjs
import { RpcClient } from "./rpc.mjs";
import { encode } from "./vendor/msgpack/index.mjs";

let failures = 0;
function check(label, ok, detail) {
  console.log(`${ok ? "PASS" : "FAIL"}  ${label}`);
  if (!ok) {
    if (detail !== undefined) console.log(`        ${detail}`);
    failures++;
  }
}

// A client whose `control` stream yields `bytes` after the request is written.
function harness() {
  let push;
  const readable = new ReadableStream({ start(c) { push = c; } });
  const writable = new WritableStream({ write() {} });
  const idle = () => ({ readable: new ReadableStream({ start() {} }), writable: new WritableStream() });
  let closedByClient = false;
  const transport = { closed: new Promise(() => {}), close() { closedByClient = true; } };
  const rpc = new RpcClient(transport, { control: { readable, writable }, proc: idle(), lsp: idle(), term: idle() });
  return { rpc, send: (b) => push.enqueue(b), closed: () => closedByClient };
}

const within = (p, ms) => Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error("TIMEOUT")), ms))]);

// A response frame `[1, 1, nil, <value>]` whose value is only the given container header.
const responseWith = (header) => new Uint8Array([0x94, 0x01, 0x01, 0xc0, ...header]);
const be32 = (n) => [(n >>> 24) & 255, (n >>> 16) & 255, (n >>> 8) & 255, n & 255];

const cases = [
  ["array", [0xdd, ...be32((1 << 22) + 1)]],
  ["map", [0xdf, ...be32((1 << 21) + 1)]],
  ["str", [0xdb, ...be32((64 << 20) + 1)]],
  ["bin", [0xc6, ...be32((64 << 20) + 1)]],
];

for (const [kind, header] of cases) {
  const h = harness();
  const req = h.rpc.request("fs_read", ["/x"]);
  h.send(responseWith(header));
  let err = null;
  try {
    await within(req, 1500);
  } catch (e) {
    err = e;
  }
  check(`oversized ${kind}: the pending request rejects (not hangs)`, err && err.message !== "TIMEOUT", `err=${err && err.message}`);
  check(`oversized ${kind}: the error names the limit`, !!err && /exceed|limit/i.test(err.message), `err=${err && err.message}`);
  const dead = await within(h.rpc.dead, 500).catch((e) => e);
  check(`oversized ${kind}: the link is dead`, h.rpc.closed && dead instanceof Error && dead.message !== "TIMEOUT");
  check(`oversized ${kind}: the transport was closed`, h.closed());
}

// Within the limits a frame still decodes: a 1000-element array response resolves.
{
  const h = harness();
  const req = h.rpc.request("fs_read", ["/x"]);
  h.send(encode([1, 1, null, Array.from({ length: 1000 }, (_, i) => i)]));
  const v = await within(req, 1500).catch((e) => e);
  check("in-limit frame: a normal response still resolves", Array.isArray(v) && v.length === 1000, `v=${v}`);
}

console.log(failures === 0 ? "\nALL PASS — the daemon wire refuses oversized msgpack frames loudly" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
