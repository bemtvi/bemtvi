//! The **bounded compute sandbox** — a second, deliberately tiny Lua VM that the
//! synchronous editing paths in `bemtvi-core` call into and use the answer from in
//! the same tick.
//!
//! It exists because the main Lua VM cannot serve those paths: it is driven
//! asynchronously from the server and reached only through the pushed mirror, so
//! a synchronous `:s` cannot await it. See [`bemtvi_core::sandbox`] for the seam
//! this implements and `docs/plans/2026-08-16-lua-sandbox-and-substitute-expressions.md`
//! for the design (including the mlua-vs-rhai measurements that chose PUC Lua).
//!
//! Three properties define it, and each is enforced here rather than documented:
//!
//! - **Pure.** Chunks run in a closed environment holding only the value-level
//!   stdlib. There is no `io`, `os`, `package`, `require`, `load`, `dofile`,
//!   `debug`, no coroutines, and no `btv.*` — so a sandbox function cannot reach
//!   editor state, the filesystem, or the network, and the
//!   Lua-reads-go-through-the-mirror invariant holds by construction.
//! - **Stateless.** The environment is read-only and `rawset` is withheld, so
//!   nothing survives from one call to the next or crosses between expressions.
//!   See [`read_only`] for why that is a feature rather than a limitation.
//! - **Bounded in time.** Every call carries a wall-clock deadline enforced by an
//!   instruction hook. `pcall`/`xpcall` are deliberately *absent*: a deadline
//!   unwinds as an ordinary Lua error, so an expression able to catch errors
//!   could swallow its own deadline and spin forever.
//! - **Bounded in memory.** The VM has a hard allocation ceiling, so
//!   `string.rep("x", 1e12)` fails the call instead of the process.

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use bemtvi_core::sandbox::{SandboxEngine, SandboxError, SandboxFn, CALL_DEADLINE};
use mlua::{Lua, Table, Value, VmState};

/// How often the deadline hook runs, in VM instructions.
///
/// The interval barely matters for cost — PUC Lua takes a slower dispatch path
/// for *every* instruction as soon as any hook is installed (measured at ~+65%
/// whether the hook fires every 200 or every 100,000 instructions), so this is
/// chosen purely for deadline *resolution*, not overhead.
const HOOK_INTERVAL: u32 = 10_000;

/// The VM's allocation ceiling (16 MiB) — generous for string munging, far below
/// anything that would threaten the editor process.
const MEMORY_LIMIT: usize = 16 * 1024 * 1024;

/// The marker a deadline abort carries, so it can be told apart from an ordinary
/// error the expression itself raised.
const DEADLINE_MARK: &str = "@btv-sandbox-deadline@";

/// The global names the sandbox environment exposes. Value-level only: nothing
/// that performs I/O, loads code, or reaches the host.
///
/// `pcall`/`xpcall` are excluded on purpose — see the module docs. So are
/// `rawset`/`rawget`: they bypass metatables, which is exactly how an expression
/// would defeat the read-only environment below and smuggle state from one call
/// (or one expression) to the next.
const SAFE_GLOBALS: &[&str] = &[
    "assert", "error", "ipairs", "next", "pairs", "rawequal", "rawlen", "select", "tonumber",
    "tostring", "type",
];

/// The stdlib tables the sandbox environment exposes. All pure.
const SAFE_LIBS: &[&str] = &["math", "string", "table", "utf8"];

/// A pure, deadline-bounded Lua VM.
pub struct LuaSandbox {
    lua: Lua,
    /// The closed environment every compiled chunk runs in.
    env: Table,
    /// Compiled chunks by handle id.
    fns: HashMap<u64, mlua::Function>,
    next_id: u64,
    /// When the in-flight call must be abandoned; `None` between calls.
    deadline: Rc<Cell<Option<Instant>>>,
}

impl LuaSandbox {
    /// Build the VM, its closed environment, and its deadline hook.
    pub fn new() -> mlua::Result<Self> {
        let lua = Lua::new();
        lua.set_memory_limit(MEMORY_LIMIT)?;

        // Build the environment explicitly rather than deleting globals from the
        // real one: an allow-list cannot be outflanked by a stdlib addition.
        let globals = lua.globals();
        let base = lua.create_table()?;
        for name in SAFE_GLOBALS {
            let v: Value = globals.get(*name)?;
            base.set(*name, v)?;
        }
        // Each stdlib table is itself frozen. They are shared by every compiled
        // chunk, so a writable `string`/`math` would be a channel between
        // unrelated expressions (and a way to corrupt the library for all of them).
        for name in SAFE_LIBS {
            let lib: Table = globals.get(*name)?;
            base.set(*name, read_only(&lua, lib)?)?;
        }

        // The environment chunks actually run in: an *empty* table whose reads fall
        // through to the allow-list and whose writes raise.
        //
        // Empty is the load-bearing part. `__newindex` only fires for a key the
        // table does not already have, so exposing the allow-list directly would
        // leave every one of its names quietly assignable. With nothing present,
        // every assignment goes through the guard.
        let env = read_only(&lua, base.clone())?;
        // `_G` is the frozen environment itself, so reaching for a global the long
        // way round lands on the same wall.
        base.set("_G", &env)?;

        let deadline: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
        let watch = deadline.clone();
        lua.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(HOOK_INTERVAL),
            move |_lua, _debug| match watch.get() {
                Some(at) if Instant::now() > at => {
                    Err(mlua::Error::RuntimeError(DEADLINE_MARK.to_string()))
                }
                _ => Ok(VmState::Continue),
            },
        )?;

        Ok(Self {
            lua,
            env,
            fns: HashMap::new(),
            next_id: 1,
            deadline,
        })
    }

    /// The compiled chunk behind a handle, or a loud error naming the call shape
    /// whose handle went stale.
    fn lookup(&self, f: SandboxFn, what: &str) -> Result<mlua::Function, SandboxError> {
        self.fns
            .get(&f.0)
            .cloned()
            .ok_or_else(|| SandboxError::Runtime(format!("{what} handle released")))
    }

    /// Map an mlua error onto the seam's typed error, recovering a deadline abort
    /// from its marker.
    fn runtime_err(err: mlua::Error) -> SandboxError {
        let msg = err.to_string();
        if msg.contains(DEADLINE_MARK) {
            SandboxError::Deadline(CALL_DEADLINE)
        } else {
            SandboxError::Runtime(tidy(&msg))
        }
    }
}

/// Trim mlua's chunk-name and traceback noise down to the message a user needs.
fn tidy(msg: &str) -> String {
    let first = msg.lines().next().unwrap_or(msg);
    // `[string "=btv-sandbox"]:1: real message` -> `real message`
    match first.rfind("]:") {
        Some(i) => first[i + 2..]
            .trim_start_matches(|c: char| c.is_ascii_digit() || c == ':')
            .trim()
            .to_string(),
        None => first.trim().to_string(),
    }
}

impl SandboxEngine for LuaSandbox {
    fn compile_expr(&mut self, src: &str, params: &[&str]) -> Result<SandboxFn, SandboxError> {
        // Wrapping the expression in a function is what makes it callable with
        // fresh arguments; the inner function inherits the sandbox environment as
        // its `_ENV` upvalue, so the closure is sandboxed too.
        let wrapped = format!("return function({}) return ({src}) end", params.join(", "));
        let outer = self
            .lua
            .load(&wrapped)
            .set_name("=btv-sandbox")
            .set_environment(self.env.clone())
            .into_function()
            .map_err(|e| SandboxError::Compile(tidy(&e.to_string())))?;
        let f: mlua::Function = outer
            .call(())
            .map_err(|e| SandboxError::Compile(tidy(&e.to_string())))?;

        let id = self.next_id;
        self.next_id += 1;
        self.fns.insert(id, f);
        Ok(SandboxFn(id))
    }

    fn call_subst(
        &mut self,
        f: SandboxFn,
        groups: &[Option<&str>],
        lnum: usize,
    ) -> Result<String, SandboxError> {
        let func = self
            .fns
            .get(&f.0)
            .ok_or_else(|| SandboxError::Runtime("expression handle released".into()))?
            .clone();

        // `m[0]` is the whole match and `m[1..]` the groups; a group that did not
        // participate is simply absent, so it reads as `nil`.
        let m = self
            .lua
            .create_table()
            .map_err(|e| SandboxError::Runtime(tidy(&e.to_string())))?;
        for (i, g) in groups.iter().enumerate() {
            if let Some(text) = g {
                m.set(i, *text)
                    .map_err(|e| SandboxError::Runtime(tidy(&e.to_string())))?;
            }
        }

        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        let result: Result<Value, mlua::Error> = func.call((m, lnum as i64));
        self.deadline.set(None);

        text_result(result.map_err(Self::runtime_err)?)
    }

    fn call_fold_text(
        &mut self,
        f: SandboxFn,
        first: &str,
        lines: i64,
        lnum: i64,
    ) -> Result<String, SandboxError> {
        let func = self
            .fns
            .get(&f.0)
            .ok_or_else(|| SandboxError::Runtime("foldtext handle released".into()))?
            .clone();
        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        let result: Result<Value, mlua::Error> = func.call((first, lines, lnum));
        self.deadline.set(None);
        text_result(result.map_err(Self::runtime_err)?)
    }

    fn call_score(
        &mut self,
        f: SandboxFn,
        label: &str,
        query: &str,
        score: i64,
    ) -> Result<f64, SandboxError> {
        let func = self
            .fns
            .get(&f.0)
            .ok_or_else(|| SandboxError::Runtime("scorer handle released".into()))?
            .clone();

        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        let result: Result<Value, mlua::Error> = func.call((label, query, score));
        self.deadline.set(None);

        number_result(result.map_err(Self::runtime_err)?)
    }

    fn call_filetype(
        &mut self,
        f: SandboxFn,
        name: &str,
        ext: &str,
        head: &str,
    ) -> Result<Option<String>, SandboxError> {
        let func = self.lookup(f, "filetype")?;
        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        let result: Result<Value, mlua::Error> = func.call((name, ext, head));
        self.deadline.set(None);
        match result.map_err(Self::runtime_err)? {
            // Declining is a normal answer: the built-in tables still apply.
            Value::Nil => Ok(None),
            other => text_result(other).map(|s| (!s.is_empty()).then_some(s)),
        }
    }

    fn call_indent(
        &mut self,
        f: SandboxFn,
        prev: &str,
        line: &str,
        lnum: i64,
        sw: i64,
        previndent: i64,
    ) -> Result<Option<i64>, SandboxError> {
        let func = self.lookup(f, "indentexpr")?;
        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        let result: Result<Value, mlua::Error> = func.call((prev, line, lnum, sw, previndent));
        self.deadline.set(None);
        match result.map_err(Self::runtime_err)? {
            // Declining hands the line to `smartindent` / `autoindent`.
            Value::Nil => Ok(None),
            Value::Integer(n) => Ok(Some(n.max(0))),
            Value::Number(n) => Ok(Some(n.max(0.0) as i64)),
            other => Err(SandboxError::BadReturn(other.type_name().to_string())),
        }
    }

    fn call_foldexpr(
        &mut self,
        f: SandboxFn,
        line: &str,
        lnum: i64,
    ) -> Result<String, SandboxError> {
        let func = self.lookup(f, "foldexpr")?;
        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        let result: Result<Value, mlua::Error> = func.call((line, lnum));
        self.deadline.set(None);
        text_result(result.map_err(Self::runtime_err)?)
    }

    fn call_complete_score(
        &mut self,
        f: SandboxFn,
        label: &str,
        query: &str,
        score: i64,
        kind: &str,
    ) -> Result<f64, SandboxError> {
        let func = self.lookup(f, "completion scorer")?;
        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        let result: Result<Value, mlua::Error> = func.call((label, query, score, kind));
        self.deadline.set(None);
        number_result(result.map_err(Self::runtime_err)?)
    }

    fn call_eval(
        &mut self,
        f: SandboxFn,
        line: &str,
        lnum: i64,
        col: i64,
    ) -> Result<String, SandboxError> {
        let func = self.lookup(f, "expression register")?;
        self.deadline.set(Some(Instant::now() + CALL_DEADLINE));
        let result: Result<Value, mlua::Error> = func.call((line, lnum, col));
        self.deadline.set(None);
        text_result(result.map_err(Self::runtime_err)?)
    }

    fn release(&mut self, f: SandboxFn) {
        self.fns.remove(&f.0);
    }
}

/// Wrap `inner` in a table that reads through to it and refuses every write.
///
/// This is what makes the sandbox genuinely stateless. Without it a chunk could
/// stash a value in a global and read it back on the next call — which sounds
/// useful and is a trap, because no call shape here is a clean once-per-item
/// traversal: `:s` re-runs the expression on every keystroke of the live
/// preview, a foldexpr sees only the rows an edit touched, the picker scorer
/// sees only the top survivors, and `foldtext` is memoized so calls are skipped
/// outright. An accumulator is wrong in all of them. Fold-level *nesting*, the
/// one case that really wants carried state, is expressed with the relative
/// values (`>N`/`<N`/`aN`/`sN`/`=`) and accumulated by the engine instead.
fn read_only(lua: &Lua, inner: Table) -> mlua::Result<Table> {
    let proxy = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", inner)?;
    meta.set(
        "__newindex",
        lua.create_function(|_, (_, key, _): (Table, Value, Value)| {
            let name = match &key {
                Value::String(s) => s.to_string_lossy().to_string(),
                other => format!("{other:?}"),
            };
            Err::<(), _>(mlua::Error::RuntimeError(format!(
                "cannot assign `{name}`: sandbox expressions are stateless \
                 (nothing carries from one call to the next)"
            )))
        })?,
    )?;
    // Hide the metatable so it cannot simply be replaced.
    meta.set("__metatable", false)?;
    proxy.set_metatable(Some(meta))?;
    Ok(proxy)
}

/// The shared result conversion for the two re-rankers, which produce a *sort
/// key*.
///
/// A sort key has to be a number: coercing a string would order rows lexically
/// and look like a working scorer that ranks nonsense.
fn number_result(v: Value) -> Result<f64, SandboxError> {
    match v {
        Value::Integer(n) => Ok(n as f64),
        Value::Number(n) => Ok(n),
        other => Err(SandboxError::BadReturn(other.type_name().to_string())),
    }
}

/// The shared result conversion for the calls that produce *text*.
///
/// A number is the one non-string result worth accepting: arithmetic on a
/// captured number is the archetypal `:s` use, and a fold's line count is a
/// natural thing to return bare. Anything else is a bug in the expression, not a
/// value to coerce.
fn text_result(v: Value) -> Result<String, SandboxError> {
    match v {
        Value::String(s) => Ok(s.to_string_lossy().to_string()),
        Value::Integer(n) => Ok(n.to_string()),
        Value::Number(n) => Ok(format_number(n)),
        other => Err(SandboxError::BadReturn(other.type_name().to_string())),
    }
}

/// Render a Lua float the way Lua's own `tostring` does, so `\=m[0] / 2` reads
/// naturally: an integral value loses its `.0`.
fn format_number(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}
