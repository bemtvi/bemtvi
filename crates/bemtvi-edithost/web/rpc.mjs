// The browser side of bemtvi's daemon wire — the JS twin of `crates/bemtvi-rpc/src/lib.rs`'s
// `Rpc` + reader task (Phase 6b). The native edit-host fulfils the daemon's `HostServices`
// over `bemtvi-rpc` + tokio; the browser Worker has neither (the whole point of `EditHost`
// is no tokio in the Worker), so the client end of the wire is reimplemented here in JS,
// driven over WebTransport (HTTP/3 / QUIC) bidi streams to a real `bemtvi --daemon --listen`.
//
// Multi-stream (see `crates/bemtvi-server/src/daemon.rs` → `LegGroup` and
// `docs/plans/2026-06-26-multi-stream-daemon-transport.md`): the legs are split across four
// bidi streams by latency class — Control (`fs_*` / `config_*` / `luafs_*`), Proc (`proc_*`),
// Lsp (`lsp_*`), Term (`term_*`) — so a `term_data` / `proc_stdout` flood can't head-of-line
// block an `fs_write` save at the QUIC layer. Each stream is prefixed with its group's
// one-byte tag (the daemon reads it to dispatch the stream); outbound `request`/`notify`
// pick their stream by method (`groupForMethod`, the JS mirror of `LegGroup::classify`).
//
// The wire (see bemtvi-rpc): bare msgpack values, no length prefix — msgpack is
// self-delimiting, so a complete value per frame. `@msgpack/msgpack`'s `decodeMultiStream`
// reads a bidi stream's `ReadableStream` and yields exactly one decoded value per frame,
// handling the split/coalesced-chunk framing for us. Frame shapes:
//   request       [0, msgid, method, params]   (edit-host → daemon; we send these)
//   response      [1, msgid, error, result]    (daemon → edit-host; resolves a request)
//   notification  [2, method, params]          (either direction, fire-and-forget)
// Request *responses* are routed by msgid (globally unique across streams), so the read loop
// of whichever stream carries the reply resolves it; the daemon's *pushes* (proc/terminal/
// fs_changed/lsp) are notifications surfaced via `onNotify`, which the Worker handles in
// `onDaemonNotify`.
import { encode, decodeMultiStream } from "./vendor/msgpack/index.mjs";

// The leg groups and their wire tag bytes — must match `LegGroup::tag` in
// `crates/bemtvi-server/src/daemon.rs`. The browser drives all four (it owns `:terminal` and
// the `btv.fs.watch` streaming leg, which the native edit-host doesn't).
const GROUP_TAGS = { control: 0, proc: 1, lsp: 2, term: 3 };

/**
 * The leg group that owns a wire method — the JS mirror of `LegGroup::classify`. The four
 * arms partition the method namespace disjointly; an unknown method defaults to Control (the
 * peer is the same build, so this is belt-and-braces).
 * @param {string} method @returns {keyof typeof GROUP_TAGS}
 */
function groupForMethod(method) {
  // dproc_* / sock_* are the duplex btv.process / btv.socket DAP transports — they ride the
  // Proc stream as process/socket siblings (matches `LegGroup::classify` in daemon.rs).
  if (method.startsWith("proc_") || method.startsWith("dproc_") || method.startsWith("sock_"))
    return "proc";
  if (method.startsWith("lsp_")) return "lsp";
  if (method.startsWith("term_")) return "term";
  return "control"; // fs_* / config_* / luafs_* and anything else
}

/** A live msgpack-RPC client over the daemon's per-group WebTransport bidi streams. */
export class RpcClient {
  /**
   * @param {WebTransport} transport
   * @param {Record<keyof typeof GROUP_TAGS, WebTransportBidirectionalStream>} streams
   */
  constructor(transport, streams) {
    this.transport = transport;
    this.nextId = 1; // globally unique across streams, so responses route unambiguously
    this.pending = new Map(); // msgid -> { resolve, reject }
    this.onNotify = null; // (method, params) => void  — daemon→client pushes
    this.closed = false;
    this.writers = {}; // group name -> WritableStreamDefaultWriter
    // Resolves the instant this link dies (a stream error, a dropped QUIC session, or an
    // explicit `close()`). The reconnect supervisor awaits it to know when to re-dial, so it
    // never has to poll `closed`. Resolved exactly once (in `_fail`).
    this.dead = new Promise((resolve) => {
      this._markDead = resolve;
    });
    // One writer + read loop per group stream. The first byte written on each stream is its
    // group tag — the daemon reads it to dispatch the stream to that group's legs (and it
    // makes the freshly-opened stream visible to the daemon's `accept_bi` promptly). The tag
    // write is enqueued before any frame, and a WritableStream preserves write order, so it
    // always precedes the group's RPC frames.
    for (const name of Object.keys(streams)) {
      const stream = streams[name];
      const writer = stream.writable.getWriter();
      this.writers[name] = writer;
      writer.write(new Uint8Array([GROUP_TAGS[name]])).catch((e) => this._fail(e));
      // The reader loop runs for the connection's lifetime; a stream error or peer close
      // ends it and we reject every in-flight request loudly (no silently-hung `:e`/`:w`).
      this._readLoop(stream.readable).catch((e) => this._fail(e));
    }
    // A dropped QUIC session must also fail in-flight work, even if a read loop is parked.
    transport.closed.then(
      () => this._fail(new Error("daemon connection closed")),
      (e) => this._fail(e),
    );
  }

  async _readLoop(readable) {
    for await (const frame of decodeMultiStream(readable)) {
      if (!Array.isArray(frame) || frame.length < 1) continue;
      const type = frame[0];
      if (type === 1) {
        // [1, msgid, error, result]
        const [, msgid, error, result] = frame;
        const p = this.pending.get(msgid);
        if (!p) continue; // unknown/duplicate msgid — drop
        this.pending.delete(msgid);
        if (error !== null && error !== undefined) {
          p.reject(new Error(typeof error === "string" ? error : JSON.stringify(error)));
        } else {
          p.resolve(result);
        }
      } else if (type === 2) {
        // [2, method, params] — a daemon push (fs_changed / proc_* / lsp_* / term_data).
        // `onNotify` may return a promise: awaiting it pauses this read loop, which stops
        // pulling the WebTransport stream → QUIC flow-control backpressures the daemon. That
        // is the browser end of terminal backpressure — without it the daemon would keep
        // sending a flood the apply side can't keep up with, and a `^C` couldn't stop it.
        if (this.onNotify) {
          const r = this.onNotify(frame[1], frame[2] ?? []);
          if (r && typeof r.then === "function") await r;
        }
      }
      // type 0 (request from the daemon) never happens — every daemon→edit-host message
      // is a response or a notification — so it is ignored.
    }
    // Clean EOF (the daemon hung up): fail any still-pending requests.
    this._fail(new Error("daemon stream ended"));
  }

  _fail(err) {
    if (this.closed) return;
    this.closed = true;
    for (const { reject } of this.pending.values()) reject(err);
    this.pending.clear();
    // Wake the reconnect supervisor (or anything else awaiting this link's death).
    this._markDead(err);
  }

  /**
   * Issue an RPC request and await its result. Rejects on an RPC error reply or a dropped
   * connection — fail loud, never a silent empty value (CLAUDE.md "No silent stubs").
   * @param {string} method @param {Array<any>} params
   */
  async request(method, params = []) {
    if (this.closed) throw new Error(`daemon connection closed (request ${method})`);
    const msgid = this.nextId++;
    const promise = new Promise((resolve, reject) => this.pending.set(msgid, { resolve, reject }));
    await this.writers[groupForMethod(method)].write(encode([0, msgid, method, params]));
    return promise;
  }

  /** Fire a notification (no reply). @param {string} method @param {Array<any>} params */
  async notify(method, params = []) {
    if (this.closed) return;
    await this.writers[groupForMethod(method)].write(encode([2, method, params]));
  }

  /**
   * Tear down this link — used when a runtime `:connect` replaces it with a new daemon.
   * Fails any in-flight request loudly (no silently-hung `:e`/`:w` on the old wire) and
   * closes the WebTransport session.
   */
  close() {
    this._fail(new Error("daemon connection replaced"));
    try {
      this.transport.close();
    } catch {
      // Already closing/closed — nothing to do.
    }
  }
}

/**
 * Dial a `bemtvi --daemon --listen` listener and return a live {@link RpcClient}.
 *
 * `uri` is the launch-printed connect string `bemtvi://HOST:PORT/TOKEN?cert=HASH`: the bearer
 * TOKEN rides the WebTransport CONNECT path (the daemon reads `request.path()`), and the
 * self-signed cert HASH (dotted-hex SHA-256) is pinned TOFU via `serverCertificateHashes`
 * — the browser twin of the native `connect_quic` (`crates/bemtvi-server/src/quic.rs`). The
 * edit-host opens one tagged bidi stream per leg group; the daemon serves each group over
 * its own stream (see {@link RpcClient}).
 */
export async function dialDaemon(uri) {
  const u = new URL(uri);
  const token = u.pathname.replace(/^\//, "");
  const certHex = new URLSearchParams(u.search).get("cert");
  if (!token) throw new Error(`daemon URI missing bearer token: ${uri}`);
  if (!certHex) throw new Error(`daemon URI missing ?cert= hash: ${uri}`);
  const hash = Uint8Array.from(certHex.split(":").map((h) => parseInt(h, 16)));
  if (hash.length !== 32 || hash.some((b) => Number.isNaN(b))) {
    throw new Error(`daemon cert hash is not 32 dotted-hex bytes: ${certHex}`);
  }

  const transport = new WebTransport(`https://${u.host}/${token}`, {
    serverCertificateHashes: [{ algorithm: "sha-256", value: hash }],
  });
  await transport.ready; // rejects loudly on a bad cert/token/transport
  // One bidi stream per leg group. The daemon dispatches by each stream's tag byte (written
  // first by `RpcClient`), not by open order, so the order here doesn't matter.
  const streams = {};
  for (const name of Object.keys(GROUP_TAGS)) {
    streams[name] = await transport.createBidirectionalStream();
  }
  return new RpcClient(transport, streams);
}
