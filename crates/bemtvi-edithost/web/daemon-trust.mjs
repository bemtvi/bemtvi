// Daemon trust + per-daemon shada scoping for the browser edit-host.
//
// A page URL can carry `?daemon=bemtvi://HOST:PORT/TOKEN?cert=HASH`, so anyone can hand the
// user a link that dials *their* daemon. A daemon session runs that daemon's `init.lua` in
// this page's Lua VM, so two rules apply:
//
//  * A daemon named by the page URL connects only after the user approves it (index.html
//    shows HOST:PORT + the cert hash). Approvals are remembered per HOST:PORT + cert hash in
//    the page origin's localStorage, so a daemon the user already trusted reconnects with no
//    prompt — and a different cert at the same address prompts again.
//  * A daemon session never sees another session's persisted state. The local serverless
//    session keeps the original OPFS blob (`/.bemtvi/shada`); each daemon gets its own blob
//    under `/.bemtvi/remote-shada/`, keyed by HOST:PORT. This mirrors the native client
//    (`prepare_remote_shada` in crates/bemtvi-server/src/shada.rs): a remote session's shada
//    belongs to the daemon it talks to and is isolated from the local global store. The key
//    leaves out the cert hash because the daemon mints a fresh self-signed cert on every
//    launch; keying on it would drop a daemon's history at each restart. The address alone
//    still isolates sessions: a link can only reach a namespace by dialing that address.
//
// No dependencies: imported by index.html, worker.mjs and the Node verifiers alike.

/** The localStorage key holding the approved `HOST:PORT|cert` identities (a JSON array). */
export const APPROVALS_KEY = "bemtvi.daemon-approvals";

/** The local (serverless) session's shada blob. */
export const LOCAL_SHADA_PATH = "/.bemtvi/shada";

/**
 * The identity of a `bemtvi://HOST:PORT/TOKEN?cert=HASH` URI: `{ hostPort, cert }`, with the
 * cert hash lowercased. Throws on a URI with no host or no `?cert=`, the same inputs
 * `dialDaemon` rejects.
 */
export function daemonIdentity(uri) {
  const u = new URL(uri);
  const cert = (new URLSearchParams(u.search).get("cert") || "").toLowerCase();
  if (!u.host) throw new Error(`daemon URI has no HOST:PORT: ${uri}`);
  if (!cert) throw new Error(`daemon URI missing ?cert= hash: ${uri}`);
  return { hostPort: u.host, cert };
}

/** The stored form of an approved identity (what `APPROVALS_KEY`'s array holds). */
export const approvalEntry = (id) => `${id.hostPort}|${id.cert}`;

function readApprovals(storage) {
  try {
    const list = JSON.parse(storage.getItem(APPROVALS_KEY) || "[]");
    return Array.isArray(list) ? list : [];
  } catch {
    return [];
  }
}

/** Whether the user already approved this daemon identity. */
export function isDaemonApproved(storage, id) {
  return readApprovals(storage).includes(approvalEntry(id));
}

/** Remember an approval, so the same HOST:PORT + cert connects without a prompt next time. */
export function rememberDaemonApproval(storage, id) {
  const list = readApprovals(storage);
  const key = approvalEntry(id);
  if (!list.includes(key)) list.push(key);
  storage.setItem(APPROVALS_KEY, JSON.stringify(list));
}

/**
 * The OPFS shada path for a session: the local blob when `uri` is empty, else the daemon's
 * own blob, named by the hex of its HOST:PORT's UTF-8 bytes (an OPFS-safe file name that
 * maps one-to-one back to the address).
 */
export function shadaPathFor(uri) {
  if (!uri) return LOCAL_SHADA_PATH;
  const { hostPort } = daemonIdentity(uri);
  const hex = [...new TextEncoder().encode(hostPort)].map((b) => b.toString(16).padStart(2, "0")).join("");
  return `/.bemtvi/remote-shada/${hex}`;
}
