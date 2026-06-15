// The browser side of nxvim's daemon wire — the JS twin of `crates/nxvim-rpc/src/lib.rs`'s
// `Rpc` + reader task (Phase 6b). The native edit-host fulfils the daemon's `HostServices`
// over `nxvim-rpc` + tokio; the browser Worker has neither (the whole point of `EditHost`
// is no tokio in the Worker), so the client end of the wire is reimplemented here in JS,
// driven over a WebTransport (HTTP/3 / QUIC) bidi stream to a real `nxvim --daemon --listen`.
//
// The wire (see nxvim-rpc): bare msgpack values, no length prefix — msgpack is
// self-delimiting, so a complete value per frame. `@msgpack/msgpack`'s `decodeMultiStream`
// reads the bidi stream's `ReadableStream` and yields exactly one decoded value per frame,
// handling the split/coalesced-chunk framing for us. Frame shapes:
//   request       [0, msgid, method, params]   (edit-host → daemon; we send these)
//   response      [1, msgid, error, result]    (daemon → edit-host; resolves a request)
//   notification  [2, method, params]          (either direction, fire-and-forget)
// Request *responses* are routed by msgid; the daemon's *pushes* (proc/lsp/fs_changed) are
// notifications surfaced via `onNotify` (unused by this slice's fs leg, ready for the next).
import { encode, decodeMultiStream } from "./vendor/msgpack/index.mjs";

/** A live msgpack-RPC client over one WebTransport bidi stream. */
export class RpcClient {
  /** @param {WebTransport} transport @param {WebTransportBidirectionalStream} stream */
  constructor(transport, stream) {
    this.transport = transport;
    this.writer = stream.writable.getWriter();
    this.nextId = 1;
    this.pending = new Map(); // msgid -> { resolve, reject }
    this.onNotify = null; // (method, params) => void  — daemon→client pushes
    this.closed = false;
    // The reader loop runs for the connection's lifetime; a stream error or peer close
    // ends it and we reject every in-flight request loudly (no silently-hung `:e`/`:w`).
    this._readLoop(stream.readable).catch((e) => this._fail(e));
    // A dropped QUIC session must also fail in-flight work, even if the read loop is parked.
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
    await this.writer.write(encode([0, msgid, method, params]));
    return promise;
  }

  /** Fire a notification (no reply). @param {string} method @param {Array<any>} params */
  async notify(method, params = []) {
    if (this.closed) return;
    await this.writer.write(encode([2, method, params]));
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
 * Dial a `nxvim --daemon --listen` listener and return a live {@link RpcClient}.
 *
 * `uri` is the launch-printed connect string `nxvim://HOST:PORT/TOKEN?cert=HASH`: the bearer
 * TOKEN rides the WebTransport CONNECT path (the daemon reads `request.path()`), and the
 * self-signed cert HASH (dotted-hex SHA-256) is pinned TOFU via `serverCertificateHashes`
 * — the browser twin of the native `connect_quic` (`crates/nxvim-server/src/quic.rs`). The
 * edit-host opens the one bidi stream; the daemon multiplexes all six wire legs over it.
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
  const stream = await transport.createBidirectionalStream();
  return new RpcClient(transport, stream);
}
