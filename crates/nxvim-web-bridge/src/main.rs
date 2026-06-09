//! `nxvim-web-bridge` entry point: resolve the `nxvim` binary, bind the listener, and
//! serve the embedded web frontend + Socket.IO relay. One executable, nothing else on
//! disk needed (the frontend is baked in for release builds). See [`nxvim_web_bridge`].

use anyhow::{Context, Result};
use nxvim_web_bridge::{app, ServerSpec};

/// Where the HTTP/Socket.IO server listens, unless `--addr HOST:PORT` overrides it.
const DEFAULT_ADDR: &str = "127.0.0.1:8000";

#[tokio::main]
async fn main() -> Result<()> {
    let addr = parse_addr()?;
    let spec = ServerSpec::resolve()?;

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    eprintln!(
        "nxvim-web-bridge: serving http://{addr} (editor: {} {})",
        spec.program.display(),
        spec.args.join(" "),
    );

    axum::serve(listener, app(spec))
        .await
        .context("server error")?;
    Ok(())
}

/// Parse the one supported flag, `--addr HOST:PORT` (default [`DEFAULT_ADDR`]). Any
/// unexpected argument is a hard error rather than a silent ignore.
fn parse_addr() -> Result<String> {
    let mut args = std::env::args().skip(1);
    let mut addr = DEFAULT_ADDR.to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => {
                addr = args
                    .next()
                    .context("--addr requires a HOST:PORT argument")?;
            }
            other => anyhow::bail!("unexpected argument: {other} (only --addr HOST:PORT)"),
        }
    }
    Ok(addr)
}
