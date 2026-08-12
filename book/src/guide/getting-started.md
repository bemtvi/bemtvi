# Getting started

## Install a pre-built binary

Grab a binary from the
[**latest release**](https://github.com/davidrios/bemtvi/releases/latest) (or the
rolling [`edge`](https://github.com/davidrios/bemtvi/releases/tag/edge) build from
`main`). Binaries are published for Linux (`x86_64`/`aarch64`, glibc), macOS
(`x86_64`/`aarch64`, signed & notarized `.pkg`/`.dmg`), and Windows (`x86_64`).
Each release ships both the terminal editor (`bemtvi`) and the native GUI
(`bemtvi-gui`); on Linux the GUI is published as a single-file, desktop-integrated
`.AppImage` (alongside a plain `.tar.gz`).

Then run it on a file (the argument is optional):

```sh
bemtvi file.txt        # terminal editor
bemtvi-gui file.txt    # native GUI (winit + wgpu)
```

Downloads ship with checksums and SLSA build provenance — see
[Verifying downloads](../appendix/verifying-downloads.md).

> **Truecolor terminal recommended.** bemtvi emits 24-bit color escapes; use a
> modern terminal with truecolor support.

The terminal editor is the whole thing in one binary: it embeds the server on
its own thread and runs the client on the main thread, joined over the same
msgpack-RPC the UI clients speak.

## Build it yourself

You need a [Rust toolchain](https://rustup.rs). Then:

```sh
# Build and run (the file argument is optional)
cargo build --release
./target/release/bemtvi file.txt

# …or straight from cargo
cargo run -p bemtvi -- file.txt

# the native GUI
cargo run -p bemtvi-gui -- file.txt
```

## Try it in the browser

A fully client-side WebAssembly build runs the entire editor core in the browser
with no server. Try the live demo at <https://bemtvi-demo.netlify.app>: use `:eo`
to open a local file, and `:setf <lang>` / `:TSInstall` to activate treesitter
highlighting for a language.
