//! The native daemon transport: a WebTransport/QUIC listener (the `--daemon --listen`
//! role) and the edit-host-side connector ([`connect_quic`]).
//!
//! Every wire leg (Phases 3c–3p) was proven over an in-process duplex, and Phase 3q
//! carried all six over one ordered **stdio** stream against the real `--daemon` binary.
//! This module is the real transport the stdio stand-in was standing in for: a
//! WebTransport/QUIC connection (Open Decision #2). The transport is the *same* stack
//! native and browser share — `wtransport` on `quinn` — so the future browser edit-host
//! (Phase 6) reaches an identical `--daemon --listen` listener.
//!
//! **Why QUIC and not ssh.** ssh stdio is one ordered TCP stream: a `HostProc` flood
//! (a fuzzy-finder's `rg`, an `npm install`) head-of-line-blocks an `HostFs` save or an LSP
//! `didChange` queued behind it, and app-level framing can't escape it (the bytes are
//! already committed to one socket). QUIC's independent streams remove that coupling at
//! the protocol level. *This slice* still multiplexes all six legs over **one** bidi
//! stream — the stdio proof carries over verbatim because [`connect_daemon`] /
//! [`run_daemon_io`] take any `AsyncRead`/`AsyncWrite` and a QUIC bidi stream's halves
//! implement exactly those. Splitting the six traffic classes onto one QUIC stream *per
//! `HostServices` class* (the actual HOL escape) is the follow-up the *Transport & stream
//! multiplexing* section describes; it builds on this listener, it does not block it.
//!
//! **Auth (Open Decision #2).** A daemon executes arbitrary processes, so an
//! unauthenticated listener is remote code execution by design — the TLS cert buys
//! *encryption, not authorization*. Two mechanisms, both minted at `--daemon --listen`
//! launch and presented by the edit-host on connect:
//!
//! - **Bearer token** — 32 CSPRNG bytes ([`mint_token`]), carried on the WebTransport
//!   CONNECT *path* (`https://host:port/<token>`). The daemon compares it
//!   constant-time and **drops the session without accepting** on a mismatch, so a bad
//!   token surfaces on the edit-host as a failed connect, never a half-open session.
//! - **Server identity — self-signed cert, TOFU pinned.** The daemon mints a self-signed
//!   [`Identity`] and prints its SHA-256 hash; the edit-host pins that hash
//!   (`with_server_certificate_hashes`) — the known-hosts model, no CA. (The browser
//!   passes the same hash to the `WebTransport` constructor, which is why the pin is a
//!   hash and not mTLS — Open Decision #2.)

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use wtransport::endpoint::endpoint_side;
use wtransport::endpoint::IncomingSession;
use wtransport::tls::Sha256Digest;
use wtransport::{
    ClientConfig, Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig,
};

use nxvim_rpc::connect;

use crate::daemon::{serve_daemon_link, DaemonClient};
use crate::run_daemon_io;

/// The SAN entries the daemon's self-signed cert is minted for. Loopback names cover the
/// local two-process split and a port-forwarded tunnel; cert-hash pinning
/// ([`connect_quic`]) verifies the *hash*, not the name, so this list is belt-and-braces
/// rather than load-bearing.
const CERT_SANS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The client's keep-alive ping interval — an editor session is mostly idle between
/// keystrokes, and without keep-alive the QUIC idle timeout would tear the (otherwise
/// healthy) connection down. Kept well under [`MAX_IDLE_TIMEOUT`].
const KEEP_ALIVE: Duration = Duration::from_secs(3);

/// How long a connection may sit with no traffic before QUIC considers it dead. The
/// effective timeout is the min of both peers' values; the client's keep-alive
/// ([`KEEP_ALIVE`]) keeps an idle-but-live session under it.
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a freshly-bound daemon listener publishes for an edit-host to connect: the bound
/// address (resolved, so an ephemeral `:0` port is concrete), the self-signed cert's
/// SHA-256 hash to pin TOFU, and the launch-minted bearer token. All three are the
/// connect credentials — the cert hash and token are the auth (see module docs).
#[derive(Clone, Debug)]
pub struct ListenerInfo {
    /// The concrete address the listener bound (ephemeral ports resolved).
    pub addr: SocketAddr,
    /// The self-signed cert's SHA-256 hash, dotted-hex — the edit-host pins this.
    pub cert_hash: String,
    /// The bearer token the edit-host presents on the CONNECT path.
    pub token: String,
}

/// Mint a fresh bearer token: 32 CSPRNG bytes, hex-encoded (URL-safe, so it rides the
/// WebTransport CONNECT path unescaped). This is the daemon's sole authorization gate, so
/// it must be unguessable — a weak token is RCE.
pub fn mint_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("CSPRNG for the daemon bearer token");
    let mut hex = String::with_capacity(64);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Bind a QUIC daemon listener on `addr` with a fresh self-signed identity, returning the
/// live [`Endpoint`] and the [`ListenerInfo`] (resolved bound address + cert hash +
/// `token`) the edit-host needs to connect. The caller drives [`serve_quic`] on the
/// returned endpoint to accept connections; splitting bind from serve lets a caller (a
/// test, or the binary) read the bound port — which `addr`'s `:0` only resolves *after*
/// the bind — before anything connects.
pub fn bind_quic_listener(
    addr: SocketAddr,
    token: String,
) -> Result<(Endpoint<endpoint_side::Server>, ListenerInfo)> {
    let identity =
        Identity::self_signed(CERT_SANS).context("minting the daemon's self-signed cert")?;
    let cert_hash = identity
        .certificate_chain()
        .as_slice()
        .first()
        .context("self-signed identity has an empty certificate chain")?
        .hash()
        .to_string();

    let config = ServerConfig::builder()
        .with_bind_address(addr)
        .with_identity(identity)
        .max_idle_timeout(Some(MAX_IDLE_TIMEOUT))
        .context("configuring the daemon listener idle timeout")?
        .build();

    let endpoint = Endpoint::server(config).context("binding the daemon QUIC listener")?;
    let bound = endpoint
        .local_addr()
        .context("resolving the daemon's bound address")?;

    Ok((
        endpoint,
        ListenerInfo {
            addr: bound,
            cert_hash,
            token,
        },
    ))
}

/// Accept connections forever, running the full six-leg [`run_daemon_io`] multiplexer over
/// each authenticated connection's single bidi stream. Each connection is served on its
/// own task, so a slow/stuck peer can't block new ones. A connection that fails auth (bad
/// or missing bearer `token`) or transport is logged to stderr (the daemon's stderr is a
/// log file in the `--connect-daemon` wiring — never the TUI) and dropped; per
/// `No silent stubs or skips`, the rejection is loud, not a silent close.
pub async fn serve_quic(endpoint: Endpoint<endpoint_side::Server>, token: String) -> Result<()> {
    let token: std::sync::Arc<str> = token.into();
    loop {
        let incoming = endpoint.accept().await;
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_one(incoming, &token).await {
                eprintln!("nxvim --daemon: connection ended: {e:#}");
            }
        });
    }
}

/// Authenticate one incoming session by its CONNECT-path bearer token, then run the
/// daemon multiplexer over the client's bidi stream until either side hangs up. A token
/// mismatch returns before `accept()`, so the session is never established — the
/// edit-host's `connect` errors rather than seeing a half-open link.
async fn serve_one(incoming: IncomingSession, token: &str) -> Result<()> {
    let request = incoming
        .await
        .context("awaiting the WebTransport session request")?;

    // The bearer token rides the CONNECT path as `/<token>`. Compare constant-time so a
    // mismatch can't be narrowed byte-by-byte via response timing.
    let presented = request.path().trim_start_matches('/');
    if !constant_time_eq(presented.as_bytes(), token.as_bytes()) {
        let remote = request.remote_address();
        // Reply 403 so the edit-host's `connect` fails *promptly and loudly* with the
        // rejection, rather than seeing a silently dropped (idle-timing-out) session.
        request.forbidden().await;
        return Err(anyhow!(
            "rejected connection from {remote}: bad or missing bearer token"
        ));
    }

    let connection = request
        .accept()
        .await
        .context("accepting the WebTransport session")?;
    // The edit-host opens the one bidi stream (`open_bi`); the daemon accepts it. All six
    // legs share it, demuxed by method namespace inside `run_daemon_io`.
    let (send, recv) = connection
        .accept_bi()
        .await
        .context("accepting the daemon bidi stream")?;
    run_daemon_io(recv, send).await
}

/// Connect to a `--daemon --listen` listener at `url` (`https://host:port`), pinning its
/// self-signed cert by `cert_hash` (TOFU) and presenting `token` on the CONNECT path, and
/// return the five edit-host seams of that one connection — the QUIC twin of
/// [`connect_daemon`](crate::connect_daemon). Blocks until the QUIC handshake +
/// WebTransport session + bidi stream are up (or fails loud if any step does), so the
/// caller has a live [`DaemonClient`] to drop into [`ServerInit`](crate::ServerInit) on
/// return.
///
/// Like `connect_daemon`, the link runs on its **own** OS thread + current-thread runtime
/// (not the server runtime): the blocking bridges park the editor/Lua thread on a `std`
/// reply channel, and that parked thread *is* the server runtime, so the wire must be
/// driven elsewhere or the park starves the reader carrying its own reply (Open Decision
/// #5's deadlock trap). The QUIC [`Endpoint`] + [`Connection`] are created on, and kept
/// alive by, that link thread for the session's lifetime.
pub fn connect_quic(url: &str, cert_hash: &str, token: &str) -> Result<DaemonClient> {
    let cert_hash: Sha256Digest = cert_hash
        .parse()
        .map_err(|e| anyhow!("invalid daemon cert hash {cert_hash:?}: {e}"))?;
    // The bearer token is the CONNECT path; the daemon reads it from `request.path()`.
    let connect_url = format!("{}/{}", url.trim_end_matches('/'), token);

    // Two std channels back to the (non-async) caller thread: the dial outcome (so a
    // failed connect/auth surfaces as an `Err` here) and then the built client (sent by
    // `serve_daemon_link`). Sends come from the link thread, recvs from this thread — no
    // self-deadlock.
    let (dial_tx, dial_rx) = std::sync::mpsc::channel::<Result<()>>();
    let (client_tx, client_rx) = std::sync::mpsc::channel::<DaemonClient>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = dial_tx.send(Err(anyhow!(e).context("building the daemon link runtime")));
                return;
            }
        };
        rt.block_on(async move {
            let (endpoint, connection, send, recv) = match quic_dial(&connect_url, cert_hash).await
            {
                Ok(parts) => parts,
                Err(e) => {
                    let _ = dial_tx.send(Err(e));
                    return;
                }
            };
            // Keep the endpoint + connection alive for the whole session; dropping either
            // would tear the QUIC link (and its streams) down.
            let _endpoint = endpoint;
            let _connection = connection;

            let (rpc, incoming) = connect(recv, send);
            // The wire is up; tell the caller, then hand off the seams and serve.
            if dial_tx.send(Ok(())).is_err() {
                return; // caller gave up waiting
            }
            serve_daemon_link(rpc, incoming, client_tx).await;
        });
    });

    dial_rx
        .recv()
        .map_err(|_| anyhow!("connect_quic: the link thread died before dialing the daemon"))??;
    client_rx
        .recv()
        .map_err(|_| anyhow!("connect_quic: the link thread died before handing off the client"))
}

/// Build the client endpoint, dial `url` (pinning `cert_hash`), and open the one bidi
/// stream the daemon multiplexer rides. Returns the endpoint + connection (the caller
/// keeps them alive) and the stream halves: the edit-host **reads** daemon replies off
/// `recv` and **writes** requests on `send`.
async fn quic_dial(
    url: &str,
    cert_hash: Sha256Digest,
) -> Result<(
    Endpoint<endpoint_side::Client>,
    Connection,
    SendStream,
    RecvStream,
)> {
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([cert_hash])
        .max_idle_timeout(Some(MAX_IDLE_TIMEOUT))
        .context("configuring the daemon client idle timeout")?
        .keep_alive_interval(Some(KEEP_ALIVE))
        .build();

    let endpoint = Endpoint::client(config).context("building the daemon QUIC client endpoint")?;
    let connection = endpoint
        .connect(url)
        .await
        .with_context(|| format!("connecting to the daemon at {url} (cert/token rejected?)"))?;
    let (send, recv) = connection
        .open_bi()
        .await
        .context("opening the daemon bidi stream")?
        .await
        .context("initializing the daemon bidi stream")?;
    Ok((endpoint, connection, send, recv))
}

/// Compare two byte slices in time independent of where they first differ (and of the
/// shared-prefix length), so the bearer-token check leaks neither the token's length
/// match nor its prefix via timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
