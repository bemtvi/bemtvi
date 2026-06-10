# nxvim-edithost-demo — ⚠️ TEMPORARY, DELETE ME

**This crate is a throwaway.** It proves exactly one thing and is meant to be
deleted once it has served its purpose.

## What it proves

That the real `nxvim-core` editor **and** the `nxvim-lua` VM (PUC Lua 5.1 + the
`vim.*` bindings) compile to `wasm32-unknown-emscripten` and **run together in one
wasm module** — you can feed vim keys, execute Lua, let Lua drive an `:`-command into
the buffer, and read the buffer back. That is the open question Phase 4 of
[`docs/plans/2026-06-09-edit-host-and-browser-lua.md`](../../docs/plans/2026-06-09-edit-host-and-browser-lua.md)
exists to answer.

## What it is NOT

It is **not** the edit-host. It hand-wires the crudest possible tie-in
(`editor.input` + `lua.eval` + drain `take_commands` into `editor.command`) and has:

- **no mirrors** → Lua cannot *read* the buffer (`nvim_buf_get_lines` won't see it),
- **no autocmds, no redraw, no async** (timers / fs / processes absent),
- **none** of `nxvim-server`'s real synchronous tick (`apply_lua_effects`, etc.).

Do **not** build features on it. The real edit-host reuses the server's glue and is a
separate, later effort (that plan). **When it lands, delete this whole directory and
its `exclude` entry in the root `Cargo.toml`.**

## Run it

```sh
rustup target add wasm32-unknown-emscripten   # once
# plus an installed + activated emsdk (provides emcc)
./build.sh         # → dist/eh.mjs + eh.wasm
node harness.mjs   # prints PASS/FAIL per assertion, exits non-zero on failure
```
