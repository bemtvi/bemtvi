//! The remote web-client bridge.
//!
//! `nxvim-web` ([`crates/nxvim-web`](../../nxvim-web)) is a fully client-side WASM
//! editor that runs *serverless* in the browser — no Lua, treesitter, or LSP, by
//! construction. Its Phase-1 [`RemoteClient`] handle can instead talk msgpack-RPC to a
//! real [`nxvim-server`], unlocking the full feature set, *if* something carries that
//! wire between the browser and a server process. This crate is that something.
//!
//! It is a standalone HTTP server that:
//!
//! 1. **Serves the frontend.** The built `crates/nxvim-web/web/` tree (HTML/CSS, the
//!    wasm-bindgen `pkg/`, vendored JS) is embedded into the binary via [`rust-embed`]
//!    in release (read from disk in debug) and served by [`static_handler`]. A
//!    `/config.json` of `{"mode":"remote"}` tells the frontend to boot in remote mode.
//! 2. **Relays each connection to its own editor.** Every browser Socket.IO connection
//!    spawns one `nxvim --server` child (the single binary's headless role — RPC over
//!    its stdin/stdout) and **byte-pumps** raw msgpack frames between the child's stdio
//!    and the socket's binary `"rpc"` events. There is no re-framing here: the wire is
//!    opaque bytes, and the browser's [`RemoteClient::feed`] reassembles frames.
//!
//! Transport is **Socket.IO** ([`socketioxide`] here, `socket.io-client` in the
//! browser) for reconnection / heartbeat / named-event framing over a single channel.
//!
//! [`nxvim-server`]: nxvim_server
//! [`RemoteClient`]: https://docs.rs/nxvim-web
//! [`RemoteClient::feed`]: https://docs.rs/nxvim-web
//! [`rust-embed`]: rust_embed

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{bail, Result};
use axum::{
    body::Body,
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use serde_json::json;
use socketioxide::{
    extract::{Data, SocketRef},
    SocketIo,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, Notify};

/// The built web frontend, embedded at compile time in release and read from disk in
/// debug. See [`static_handler`] and `build.rs`.
#[derive(rust_embed::RustEmbed)]
#[folder = "../nxvim-web/web"]
struct WebAssets;

/// How to launch the per-connection editor: the `nxvim` binary plus its argv (always
/// `--server`, the headless RPC-over-stdio role). Cloned once per incoming connection.
#[derive(Clone, Debug)]
pub struct ServerSpec {
    /// Path to the `nxvim` binary.
    pub program: PathBuf,
    /// Arguments — `["--server"]` for the real binary; tests point `program` at a stub
    /// and may pass none.
    pub args: Vec<String>,
}

impl ServerSpec {
    /// Locate the `nxvim` binary to drive headless, failing loud if it can't be found
    /// (the project's no-silent-stubs rule — a bridge that can't spawn an editor is
    /// useless, so say so at startup rather than per-connection).
    ///
    /// Resolution order: `$NXVIM_SERVER_BIN` (the path to the `nxvim` binary) → a
    /// sibling `nxvim` next to this bridge executable → `nxvim` on `PATH`. The role
    /// flag `--server` is always appended.
    pub fn resolve() -> Result<Self> {
        let exe_name = if cfg!(windows) { "nxvim.exe" } else { "nxvim" };

        if let Some(path) = std::env::var_os("NXVIM_SERVER_BIN") {
            let program = PathBuf::from(path);
            if !program.is_file() {
                bail!(
                    "NXVIM_SERVER_BIN points at {}, which is not a file",
                    program.display()
                );
            }
            return Ok(Self::with_server_flag(program));
        }

        if let Ok(exe) = std::env::current_exe() {
            if let Some(sibling) = exe.parent().map(|dir| dir.join(exe_name)) {
                if sibling.is_file() {
                    return Ok(Self::with_server_flag(sibling));
                }
            }
        }

        if let Some(found) = find_in_path(exe_name) {
            return Ok(Self::with_server_flag(found));
        }

        bail!(
            "could not locate the `nxvim` binary — set $NXVIM_SERVER_BIN to its path, \
             place the bridge next to `nxvim`, or put `nxvim` on PATH"
        );
    }

    fn with_server_flag(program: PathBuf) -> Self {
        Self {
            program,
            args: vec!["--server".to_string()],
        }
    }
}

/// Search `$PATH` for an executable named `name`, returning the first hit.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Build the axum [`Router`]: the Socket.IO transport layer (each connection spawns
/// and relays its own editor), the `/config.json` mode probe, and the embedded-asset
/// fallback. `spec` is cloned once per connection to spawn the child.
pub fn app(spec: ServerSpec) -> Router {
    let spec = Arc::new(spec);
    let (layer, io) = SocketIo::new_layer();

    io.ns("/", move |socket: SocketRef| {
        let spec = spec.clone();
        async move { on_connect(socket, spec).await }
    });

    Router::new()
        .route("/config.json", get(config_json))
        .fallback(static_handler)
        // The layer owns the `/socket.io/` engine.io endpoint; everything else falls
        // through to the routes above.
        .layer(layer)
}

/// `{"mode":"remote"}` — the frontend boots its remote path when it sees this (a
/// missing/`local` value keeps it serverless).
async fn config_json() -> Json<serde_json::Value> {
    Json(json!({ "mode": "remote" }))
}

/// Serve an embedded frontend asset by request path (`/` → `index.html`), with a
/// `Content-Type` from the file extension. Anything not in the bundle is a 404.
async fn static_handler(uri: Uri) -> Response {
    let path = match uri.path().trim_start_matches('/') {
        "" => "index.html",
        other => other,
    };

    match WebAssets::get(path) {
        Some(file) => Response::builder()
            .header(header::CONTENT_TYPE, content_type(path))
            .body(Body::from(file.data.into_owned()))
            .expect("static asset response is always well-formed"),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Pick a `Content-Type` for an asset path. `.wasm` is forced to `application/wasm` so
/// browsers stream-compile it; everything else defers to `mime_guess`'s extension
/// table (octet-stream when unknown).
fn content_type(path: &str) -> String {
    if path.ends_with(".wasm") {
        return "application/wasm".to_string();
    }
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .to_string()
}

/// A new browser connected: wire the Socket.IO event handlers to a per-connection
/// editor relay.
///
/// The transport-specific glue lives here; the byte pump itself is [`relay_connection`]
/// (so it is testable without a live socket). Each binary `"rpc"` event feeds the
/// relay's inbound channel; `on_disconnect` signals shutdown; the relay's outbound
/// chunks are emitted back as `"rpc"` events. When the relay ends (editor exit,
/// disconnect, or a send failure) the socket is torn down.
async fn on_connect(socket: SocketRef, spec: Arc<ServerSpec>) {
    // client → server: the `"rpc"` handler sends frames here; the relay drains them
    // into the editor's stdin. Unbounded is fine — input frames are tiny.
    let (to_child_tx, to_child_rx) = mpsc::unbounded_channel::<Bytes>();
    // Disconnect → tear the relay (and its child) down.
    let shutdown = Arc::new(Notify::new());

    socket.on("rpc", move |Data(frame): Data<Bytes>| {
        let to_child_tx = to_child_tx.clone();
        async move {
            let _ = to_child_tx.send(frame);
        }
    });

    let disconnect_shutdown = shutdown.clone();
    socket.on_disconnect(move || {
        let shutdown = disconnect_shutdown.clone();
        async move { shutdown.notify_one() }
    });

    // The relay outlives this handler (it runs for the whole connection), so spawn it
    // and let the connect handler return to complete the handshake.
    let emit_socket = socket.clone();
    tokio::spawn(async move {
        // Editor stdout chunk → client, raw (no re-framing — the browser's
        // `RemoteClient::feed` reassembles). A send error means the socket is gone.
        let emit = move |chunk: &[u8]| {
            emit_socket
                .emit("rpc", &Bytes::copy_from_slice(chunk))
                .is_ok()
        };
        if let Err(err) = relay_connection(&spec, to_child_rx, emit, shutdown).await {
            eprintln!("nxvim-web-bridge: relay error: {err}");
        }
        // The editor exited (or we tore down): make sure the browser sees a disconnect
        // rather than a silent stall.
        let _ = socket.disconnect();
    });
}

/// Spawn one `nxvim --server` child and run its bidirectional byte relay until the
/// editor exits, the client disconnects (`shutdown`), or a pump fails.
///
/// Transport-agnostic by design: the Socket.IO layer supplies `inbound` (client→server
/// frames) and `emit` (server→client chunks, returning `false` when the sink is gone),
/// so tests can drive it with an in-memory channel and a collector instead of a live
/// socket. The two pumps race — whichever ends first ([`pump_to_client`] on editor EOF
/// / shutdown / send-failure, or [`pump_to_child`] on the inbound channel closing) ends
/// the connection, and the child is reaped (also guaranteed by `kill_on_drop`).
pub async fn relay_connection<E>(
    spec: &ServerSpec,
    inbound: mpsc::UnboundedReceiver<Bytes>,
    emit: E,
    shutdown: Arc<Notify>,
) -> Result<()>
where
    E: Fn(&[u8]) -> bool + Send,
{
    let mut child = Command::new(&spec.program)
        .args(&spec.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr is inherited so the editor's own diagnostics land in the bridge log.
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| anyhow::anyhow!("failed to spawn {}: {err}", spec.program.display()))?;

    let stdin = child.stdin.take().expect("child stdin was piped");
    let stdout = child.stdout.take().expect("child stdout was piped");

    tokio::select! {
        _ = pump_to_client(stdout, emit, &shutdown) => {}
        _ = pump_to_child(stdin, inbound) => {}
    }

    let _ = child.kill().await;
    Ok(())
}

/// Drain client→server frames into the editor's stdin. Ends when the inbound channel
/// closes (the socket is gone) or a write fails (the editor is gone); dropping `stdin`
/// then closes it, which is how a disconnect propagates EOF to the editor.
async fn pump_to_child<W>(mut stdin: W, mut inbound: mpsc::UnboundedReceiver<Bytes>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = inbound.recv().await {
        if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
            break;
        }
    }
}

/// Pump editor-stdout chunks to the client via `emit` (raw bytes, no re-framing) until
/// EOF, a read error, a send failure (`emit` returns `false`), or `shutdown`.
async fn pump_to_client<R, E>(mut stdout: R, emit: E, shutdown: &Notify)
where
    R: AsyncRead + Unpin,
    E: Fn(&[u8]) -> bool,
{
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            _ = shutdown.notified() => break,
            read = stdout.read(&mut buf) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !emit(&buf[..n]) {
                        break;
                    }
                }
            }
        }
    }
}
