# Lua micro-benchmarks — PUC Lua (5.4, the default) vs alternative backends

A backend-agnostic suite of the operations plugins do on hot paths, exposed as
`:bench1` … `:bench10` and `:benchall`. Run the **same** `init.lua` under each
backend and compare; each result line prints a `chk=` checksum so you can confirm
both VMs did identical work (same checksum ⇒ the only difference is speed).

## What each benchmark exercises

| Cmd | Workload | Plugin hot path it stands in for |
|-----|----------|----------------------------------|
| `:bench1` | Lua-pattern tokenizing (`gmatch %a+`) | syntax / comment / completion scanning |
| `:bench2` | `string.find` sweeps (plain + pattern) | grep-style search, matchers |
| `:bench3` | `gsub` transforms (function replacement) | text munging, path/name normalization |
| `:bench4` | `table.concat` string building | statusline / virt-text assembly |
| `:bench5` | `table.sort` of records w/ comparator | ordering picker / quickfix results |
| `:bench6` | hash-table insert + lookup | dedup, "seen" sets, memo caches |
| `:bench7` | closure call overhead | callback-heavy iterator code |
| `:bench8` | metatable OOP dispatch | the class systems plugins are built on |
| `:bench9` | fuzzy subsequence scoring (byte-level) | fzf/telescope candidate ranking |
| `:bench10` | coroutine create/resume churn | async runners / generators |

Edit `SCALE` at the top of `init.lua` to make every bench run longer/shorter
(it multiplies all iteration counts equally, so the A/B stays fair).

> **Always build `--release`.** Debug timings are dominated by unoptimized
> interpreter overhead and don't reflect real performance (or even the right
> relative ordering between backends).

## 1) Baseline: PUC Lua 5.4 (the default build)

```sh
BEMTVI_CONFIG=examples/bench cargo run --release -p bemtvi -- examples/bench/sample.txt
```

In the editor: `:benchall`, then `:messages` to read the full table. The header
line shows `[Lua 5.4]`.

## 2) Switch to Luau, rebuild, run the same suite

Luau is a sandboxed dialect: it ships without `os.getenv`, `io`, `load`,
`package`, and `debug.getinfo`, so the editor needs two changes to boot — flip
the backend and re-expose the handful of stdlib bits the prelude uses from Rust.

**a. Point mlua at Luau** — in the root `Cargo.toml`, change the `mlua` line:

```toml
# from (the default):
mlua = { version = "=0.11.6", features = ["vendored", "serde", "lua54"] }
# to:
mlua = { version = "=0.11.6", features = ["vendored", "serde", "luau"] }
```

**b. Add the stdlib shims** — in `crates/bemtvi-lua/src/runtime.rs`, add the call
right after the VM is created in `LuaRuntime::new`:

```rust
        let lua = unsafe { Lua::unsafe_new_with(libs, LuaOptions::default()) };
        luau_stdlib_shims(&lua)?; // re-expose Luau's sandboxed-away stdlib
```

…and add this free function (it is a no-op under PUC — every shim only fills a
`nil` hole, so the same source builds on both backends):

```rust
/// Luau ships a deliberately sandboxed stdlib. bemtvi owns Rust implementations of
/// fs/system, so the missing surface is bridgeable; this fills the gaps the
/// prelude needs to boot. No-op under the PUC `lua54` backend (every shim guards
/// on nil). NOTE: `require`/`package.path` resolution is NOT wired here — Luau's
/// prefix-based require is a separate, larger rework; plain runtimepath `require`
/// won't resolve until that's done.
fn luau_stdlib_shims(lua: &Lua) -> mlua::Result<()> {
    let g = lua.globals();

    let os: Table = match g.get("os")? {
        mlua::Value::Table(t) => t,
        _ => {
            let t = lua.create_table()?;
            g.set("os", &t)?;
            t
        }
    };
    if matches!(os.get("getenv")?, mlua::Value::Nil) {
        os.set(
            "getenv",
            lua.create_function(|_, name: String| Ok(std::env::var(name).ok()))?,
        )?;
    }

    if matches!(g.get("load")?, mlua::Value::Nil) {
        if let mlua::Value::Function(f) = g.get("loadstring")? {
            g.set("load", f)?;
        }
    }

    if matches!(g.get("package")?, mlua::Value::Nil) {
        let pkg = lua.create_table()?;
        pkg.set("path", "")?;
        pkg.set("cpath", "")?;
        pkg.set("loaded", lua.create_table()?)?;
        g.set("package", pkg)?;
    }

    if let mlua::Value::Table(dbg) = g.get("debug")? {
        if matches!(dbg.get("getinfo")?, mlua::Value::Nil) {
            if let mlua::Value::Function(_) = dbg.get("info")? {
                let info: mlua::Value = dbg.get("info")?;
                let shim = lua
                    .load(
                        r#"
                        local info = ...
                        return function(level, _what)
                          local ok, src, line, name = pcall(info, (type(level)=='number' and level+1 or 1), "sln")
                          if not ok then return nil end
                          return { source = src and ("@"..src) or "?", short_src = src or "?",
                                   currentline = line or -1, name = name, what = "Lua" }
                        end
                        "#,
                    )
                    .call::<mlua::Function>(info)?;
                dbg.set("getinfo", shim)?;
            }
        }
    }

    Ok(())
}
```

Then rebuild and run the identical command:

```sh
BEMTVI_CONFIG=examples/bench cargo run --release -p bemtvi -- examples/bench/sample.txt
```

`:benchall` now reports `[Luau]`. Compare the two tables.

To go back to PUC: revert the `Cargo.toml` line (the `runtime.rs` shim can stay —
it's inert under PUC).

## Reading the numbers

- **`us/it`** (microseconds per iteration) is the comparable figure — same `SCALE`
  and same iteration counts on both backends, so lower is faster.
- **`chk=`** must match between the two runs for a given bench. A mismatch means
  the backends computed different results (a real dialect difference) — that
  bench's timing isn't a clean apples-to-apples comparison until reconciled.
- The benches are pure-VM CPU work; they don't touch the editor's Rust bridges,
  so they isolate the interpreter itself.
