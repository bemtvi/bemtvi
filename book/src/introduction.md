# Introduction

**nxvim** is a modal, vim-style editor written in Rust. It is a headless, asynchronous editor **server** with thin UI
**clients** (a terminal client, a native GPU GUI, and a client-side
WebAssembly build) talking over msgpack-RPC. The editor logic lives
in one place; every front end shares identical editing behavior.

It speaks vim at the keyboard — keystrokes, modes, ex-commands, and options
track [vim/neovim](https://neovim.io)'s observable editing behavior — but every
API is nxvim's own. Configuration and plugins target the **`nx.*` Lua
namespace**, where the server owns every UI surface and plugins provide data and
behavior. There are a few `vim.*` aliases over the native `nx.*` API for convenience.

## How to read this book

- **[Getting started](guide/getting-started.md)** — install or build nxvim and
  run it.
- **[Configuration](guide/configuration.md)** — point nxvim at your `init.lua`
  and set options through `nx.*`.
- **[Plugin Development](plugins/overview.md)** — the anatomy of an nxvim plugin
  and a worked example.
- **[nx.* API Reference](api/index.md)** — the public Lua API, **generated
  directly from the prelude source** so it always matches the running editor.
- **[Architecture](architecture/overview.md)** — the crate layout, client-server
  model, RPC and `View` protocols, the rope text model, and the Lua bridge.

This book is itself generated from the repository: the narrative chapters and
the long-form architecture and plugin-authoring docs come from
[`docs/`](https://github.com/davidrios/nxvim/tree/main/docs), and the API
reference is extracted from
[`crates/nxvim-lua/src/prelude/`](https://github.com/davidrios/nxvim/tree/main/crates/nxvim-lua/src/prelude).
