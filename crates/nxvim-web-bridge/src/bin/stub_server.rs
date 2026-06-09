//! Test-only stand-in for `nxvim --server`: a stdio child the bridge can spawn and
//! relay without a real editor (or its Lua/treesitter/LSP weight). The bridge's
//! integration tests point `$NXVIM_SERVER_BIN` at this binary.
//!
//! Protocol: block until the client sends *any* byte (its `nvim_ui_attach` frame),
//! then reply with a canned `redraw` notification written in **two** flushes with a
//! pause between — so the bridge's stdout pump forwards it as separate chunks and the
//! test exercises frame reassembly across them. Then drain stdin until it closes (EOF
//! when the browser disconnects) and exit. It ignores argv (the bridge passes
//! `--server`).

use std::io::{Read, Write};
use std::thread::sleep;
use std::time::Duration;

/// A complete msgpack-RPC notification frame: `[2, "redraw", [{}]]`.
/// `93`=array(3) `02`=2 `A6`+"redraw"=fixstr(6) `91`=array(1) `80`=map(0).
const CANNED_REDRAW: &[u8] = &[
    0x93, 0x02, 0xA6, b'r', b'e', b'd', b'r', b'a', b'w', 0x91, 0x80,
];

fn main() {
    let mut stdin = std::io::stdin();

    // Wait for the first client byte (the attach frame). EOF before that → nothing to
    // do.
    let mut probe = [0u8; 1];
    if stdin.read(&mut probe).unwrap_or(0) == 0 {
        return;
    }

    // Reply, split mid-frame across two flushes to force multi-chunk forwarding.
    let mut stdout = std::io::stdout();
    let (head, tail) = CANNED_REDRAW.split_at(CANNED_REDRAW.len() / 2);
    let _ = stdout.write_all(head).and_then(|_| stdout.flush());
    sleep(Duration::from_millis(20));
    let _ = stdout.write_all(tail).and_then(|_| stdout.flush());

    // Keep stdin open (and the process alive) until the bridge closes it on disconnect.
    let mut sink = Vec::new();
    let _ = stdin.read_to_end(&mut sink);
}
