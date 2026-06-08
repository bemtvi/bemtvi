# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

nxvim is a neovim clone in Rust: a headless, async editor **server** with thin UI **clients**, talking over nxvim's own msgpack-RPC. The authoritative design doc is **[docs/architecture.md](docs/architecture.md)** — read it first for the crate layout, client-server model, the RPC + `View` protocols, the rope text model, the Lua bridge, and the roadmap. This file only adds the commands and the conventions that aren't obvious from the code itself.

## Commands

```sh
# Build / run
cargo build                         # debug build of the whole workspace
cargo build --release               # release profile uses thin LTO (slower to build)
cargo run -p nxvim -- file.txt      # run the editor; the file argument is optional

# Test — everything is a black-box integration test (see "Conventions" below)
cargo test --workspace
cargo test -p nxvim-server --test editing <name>   # run a single test by name / substring

# Lint & format — both are enforced by the pre-commit hook
cargo fmt --all                     # format in place ( add `-- --check` to verify only )
cargo clippy --all-targets -- -D warnings
```

**Do not use `--all-features`** for `clippy` or `test`. The Lua backend is a Cargo feature with mutually-exclusive variants (`luajit` is the default, `lua51` the alternative); `--all-features` enables both at once, which makes `mlua-sys` fail its build script (`You can enable only one of the features: …`). Lint and test on the **default features** (LuaJIT) as shown above. To check the other backend explicitly, swap it in deliberately (`--no-default-features --features lua51`), never via `--all-features`.

A pre-commit hook (`.pre-commit-config.yaml`) runs `cargo fmt --check` + `cargo clippy -D warnings` on every commit. After a fresh clone, run `pre-commit install` once to enable it; bypass in an emergency with `git commit --no-verify`.

`vendor/neovim` is a git submodule kept purely as a behavioral/source-layout reference — it is never built or linked. It is not needed to build nxvim; populate it only if you want the reference: `git submodule update --init vendor/neovim`.

## Workflow

- **Bug fixes are test-driven.** For every bug-fix request, first write a test that *fails* while the bug exists and *passes* once the fix is in place. Confirm it fails, implement the fix, confirm it passes, then coordinate with the requester to validate the behavior. A test written before the fix proves the bug is real and that the fix isn't a no-op, and it guards against regression. (See the testing conventions below for where tests live.)

## Conventions that will bite you if missed

- **No silent stubs or skips.** Every unimplemented function, missing piece, or unsupported path must fail *loud* — raise/error with the name of what's missing — never return a fake/empty value or silently no-op. A stub that quietly succeeds makes broken behavior look like working behavior (e.g. an LSP server that "starts" but ignores its config). If something can't be done yet, say so at runtime. (See `docs/plans/2026-06-05-lsp-completion.md` for the LSP application of this.)
- **No unit tests.** Behavior is verified end-to-end through the running server: a test starts a real server over an in-process RPC pipe, feeds vim key-notation via `nvim_input`, and asserts on `nvim_buf_get_lines` / cursor / the `redraw` view. Put new coverage in `crates/nxvim-server/tests/editing.rs` (helpers: `start`, `feed`, `lines`, `cursor`) — do **not** add `#[test]` unit tests inside the crates. (Rationale: architecture.md → *Testing philosophy*.)
- **Redraw test helpers must take the *latest* queued `redraw`, never the first.** A `redraw_after(keys)`-style helper feeds input, sends an `nvim_get_mode` barrier, then reads a `redraw` off the `incoming` notification channel. The client's reader task ferries the wire into `incoming` asynchronously, so under load (`cargo test --workspace` runs many binaries while compiling) it lags and a stale frame — the startup repaint, or a previous call's trailing barrier repaint — can sit *ahead of* this input's frame. Taking the first returns the stale one → flaky, test-shuffling failures (a different test fails each run; passes serially / with `--test-threads=4`). Drain to the most recent redraw (`drain_to_latest_redraw`) with a bounded poll-retry instead of panicking on the first empty channel. The barrier's trailing repaint is state-identical for *persistent* state (message / lines / flags), so "latest" is correct — **except** one-shot transient fields (the `scroll` gesture) live only in the input's frame; for those keep the latest redraw matching a predicate (e.g. `scroll_after` → `scroll(map).is_some()`). This is a client-side harness race, **not** a server `tokio::select!` ordering issue, so `biased;` does not fix it. (Copy the take-latest pattern already in `buffers.rs` / `lsp.rs` / `screen.rs`; don't reintroduce a take-first variant.)
- **`nxvim-core` stays pure and synchronous** — no async, no I/O beyond `Buffer` file read/write, no transport types. All async / RPC / Lua lives in `nxvim-server` and above, so every front end shares identical editing behavior.
- **Dependencies are pinned exactly** (`=x.y.z`) in the root `Cargo.toml` under `[workspace.dependencies]`. Add a new dependency there with an exact version, then pull it into a crate with `<dep>.workspace = true`.
- **Indexing is byte-offset based and the rope always keeps a trailing `\n`** (so `line_count == rope.len_lines() - 1`; the final phantom line is never edited or shown). Call `Buffer::normalize()` after mutations to preserve the invariant. (Details: architecture.md → *Text model*.)

## Key files

- `crates/nxvim/src/main.rs` — wires the embedded server (its own OS thread) and the TUI client (main thread) together over a `tokio::io::duplex` pipe, using the same RPC a remote client would.
- `crates/nxvim-server/src/lib.rs` — the `run()` loop; the server is split into focused sibling modules: `dispatch.rs` (the `dispatch()` match defining the `nvim_*` RPC surface), `redraw.rs` (`redraw()` projects the editor `View` into the notification map clients render), plus `evloop.rs` / `excmd.rs` / `input.rs` / `keymap.rs` / `lifecycle.rs` / `effects.rs` / `treesitter.rs` and the `lsp/` subtree.
- `crates/nxvim-core/src/editor/` — the synchronous key / ex-command state machine, where the bulk of the editing logic lives. `mod.rs` holds the `Editor` struct and shared state; the behavior is split into per-concern submodules: `ex.rs` (ex-commands), `command.rs`, `cmdline.rs`, `motions.rs`, `operators.rs`, `insert.rs`, `cursor.rs`, `buffers.rs`, `windows.rs`, `tabs.rs`, `search.rs`, `panel.rs`, `syntax.rs`, `options.rs`, `undo.rs`.
