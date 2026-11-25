//! Generating a parser from a grammar's `grammar.js`, in-process: no `tree-sitter`
//! CLI and no `node`.
//!
//! A grammar repo that commits only `grammar.js` (no generated `src/parser.c`) is
//! built in two steps:
//!
//!   1. [`grammar_json`] evaluates `grammar.js` in an embedded QuickJS, with
//!      tree-sitter's own grammar DSL (`generate/dsl.js`, taken verbatim from the
//!      `tree-sitter-generate` crate, MIT), and returns the grammar as JSON;
//!   2. [`generate`] writes it to `src/grammar.json` and hands it to
//!      `tree-sitter-generate`, which writes `src/parser.c`, `src/node-types.json`
//!      and the `src/tree_sitter/*.h` headers a scanner includes.
//!
//! `tree-sitter-generate` has a QuickJS host of its own (its `qjs-rt` feature), but
//! that turns on `rquickjs`'s bindgen, which needs libclang on every machine that
//! builds bemtvi. This host runs on `rquickjs`'s pregenerated bindings instead. It
//! also differs where an editor needs it to: modules resolve from the grammar's
//! source tree rather than the process's working directory (the editor's cwd is the
//! user's, and installs run on a worker thread), nothing outside that tree can be
//! read, and the environment isn't exposed.
//!
//! The JS sandbox: QuickJS has no file or network access of its own. The only way out
//! is the two functions this host defines — resolve a module name and read a file —
//! and both refuse any path outside `root`, the unpacked source tree. The evaluation
//! is bounded in memory, stack and time. The Rust side's own reads and writes
//! (`src/grammar.json`, the generated files) stay inside the tree because the unpack
//! refuses a symlink that leads out of it (`install::unpack_tar_gz`).

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use rquickjs::loader::{ImportAttributes, Loader, Resolver};
use rquickjs::{Context, Ctx, Function, Module, Runtime, Value};

/// tree-sitter's grammar DSL (`grammar`, `seq`, `choice`, …). With `native` set it
/// imports the grammar from `__ts_grammar_path` and leaves the JSON in
/// `globalThis.output`.
const DSL: &str = include_str!("generate/dsl.js");

/// CommonJS `require` for grammars, over the host's `__bemtvi_resolve` /
/// `__bemtvi_read`. Each module gets its own `require` resolving from its own
/// directory, modules are cached by path, and a `.json` module is its parsed value.
/// Required files run as sloppy-mode scripts, as under node; the grammar file itself
/// is imported by `dsl.js` as a module, so it runs in strict mode (as with
/// `tree-sitter generate --js-runtime native`).
const REQUIRE_PRELUDE: &str = r#"
(function () {
  const cache = Object.create(null);
  function makeRequire(fromFile) {
    return function require(name) {
      name = String(name);
      const path = __bemtvi_resolve(fromFile, name);
      if (path === undefined) {
        throw new Error("Cannot find module '" + name + "' required from " + fromFile +
          " (a grammar is evaluated from its own source tree: node built-ins and " +
          "packages from node_modules are not available)");
      }
      if (path in cache) return cache[path].exports;
      const src = __bemtvi_read(path);
      const module = { exports: {} };
      cache[path] = module;
      if (path.endsWith(".json")) {
        module.exports = JSON.parse(src);
        return module.exports;
      }
      const fn = (0, eval)("(function (exports, require, module, __filename, __dirname) {" +
        src + "\n})");
      fn.call(module.exports, module.exports, makeRequire(path), module, path,
        __bemtvi_dirname(path));
      return module.exports;
    };
  }
  globalThis.require = makeRequire(__ts_grammar_path);
  globalThis.module = { exports: {} };
  globalThis.process = { env: {} };
  const quiet = function () {};
  globalThis.console = { log: quiet, info: quiet, warn: quiet, error: quiet, debug: quiet };
})();
"#;

/// QuickJS heap limit. A grammar's evaluation builds one rule tree; the largest
/// grammars need a few tens of MB.
const MEMORY_LIMIT: usize = 256 * 1024 * 1024;
/// QuickJS stack limit, for the DSL's recursion over deeply nested rules.
const STACK_LIMIT: usize = 1024 * 1024;
/// How long `grammar.js` may run. Real grammars evaluate in well under a second; a
/// runaway one would otherwise pin a core and wedge the install for good (and the
/// editor's exit, which waits for blocking work). `$BEMTVI_GRAMMAR_JS_TIMEOUT_MS`
/// overrides it.
const JS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Generate `src/parser.c` (plus `node-types.json` and the `tree_sitter/` headers)
/// for the grammar at `grammar_root`. A committed `src/grammar.json` is generated
/// from directly; otherwise `grammar.js` is evaluated first. `root` is the unpacked
/// source tree: what a `require` / `import` in `grammar.js` may reach (a monorepo's
/// grammar can pull shared code from a sibling directory).
pub(crate) fn generate(grammar_root: &Path, root: &Path) -> Result<()> {
    // One generation at a time. `:TSInstall a b c` installs on parallel workers, and a
    // big grammar's generation takes hundreds of MB and a core for seconds (minutes in
    // a debug build); several at once multiply that for no gain. A poisoned lock (a
    // generation panicked) holds no state, so it's taken anyway.
    static GENERATING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one_at_a_time = GENERATING.lock().unwrap_or_else(|e| e.into_inner());

    let src_dir = grammar_root.join("src");
    let json_path = src_dir.join("grammar.json");
    if !json_path.exists() {
        let grammar_js = grammar_root.join("grammar.js");
        if !grammar_js.exists() {
            bail!(
                "neither src/parser.c, src/grammar.json nor grammar.js at {} — nothing to build",
                grammar_root.display()
            );
        }
        let json = grammar_json(&grammar_js, root)?;
        std::fs::create_dir_all(&src_dir)
            .with_context(|| format!("create {}", src_dir.display()))?;
        // Everything below writes into `src/`. The unpack refuses symlinks that lead out
        // of the tree; this checks the one directory that matters again, on disk.
        let real_src = canonical(&src_dir)?;
        let real_root = canonical(root)?;
        if !real_src.starts_with(&real_root) {
            bail!(
                "{} resolves to {}, outside the grammar's source tree; refusing to write there",
                src_dir.display(),
                real_src.display()
            );
        }
        std::fs::write(&json_path, json)
            .with_context(|| format!("write {}", json_path.display()))?;
    }
    tree_sitter_generate::generate_parser_in_directory(
        grammar_root,
        None::<PathBuf>,
        Some(json_path.clone()),
        tree_sitter::LANGUAGE_VERSION,
        None,
        None,
        true,
        tree_sitter_generate::OptLevel::default(),
    )
    .map_err(|e| anyhow::anyhow!("generate a parser from {}: {e}", json_path.display()))?;
    if !src_dir.join("parser.c").exists() {
        bail!(
            "generating from {} produced no src/parser.c",
            json_path.display()
        );
    }
    Ok(())
}

/// Evaluate `grammar_js` with tree-sitter's DSL and return the grammar as JSON.
/// `root` bounds what the grammar may `require` / `import`.
pub(crate) fn grammar_json(grammar_js: &Path, root: &Path) -> Result<String> {
    let root = canonical(root)?;
    let grammar_js = canonical(grammar_js)?;
    if !grammar_js.starts_with(&root) {
        bail!(
            "{} is outside the grammar's source tree {}",
            grammar_js.display(),
            root.display()
        );
    }
    let grammar_name = path_str(&grammar_js)?;

    let runtime = Runtime::new().context("start QuickJS")?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    runtime.set_loader(
        TreeResolver { root: root.clone() },
        TreeLoader { root: root.clone() },
    );
    let timeout = std::env::var("BEMTVI_GRAMMAR_JS_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(JS_TIMEOUT);
    let deadline = std::time::Instant::now() + timeout;
    let timed_out = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = timed_out.clone();
    // QuickJS calls this regularly while running JS; `true` raises an uncatchable error.
    runtime.set_interrupt_handler(Some(Box::new(move || {
        let over = std::time::Instant::now() >= deadline;
        if over {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        over
    })));
    let context = Context::full(&runtime).context("create a QuickJS context")?;

    let result = context.with(|ctx| -> Result<String> {
        let globals = ctx.globals();
        js(&ctx, globals.set("native", true))?;
        js(&ctx, globals.set("__ts_grammar_path", grammar_name.clone()))?;

        let resolve_root = root.clone();
        js(
            &ctx,
            globals.set(
                "__bemtvi_resolve",
                js(
                    &ctx,
                    Function::new(
                        ctx.clone(),
                        move |from: String, name: String| -> Option<String> {
                            resolve_module(&resolve_root, Path::new(&from), &name)
                                .and_then(|p| path_str(&p).ok())
                        },
                    ),
                )?,
            ),
        )?;
        let read_root = root.clone();
        js(
            &ctx,
            globals.set(
                "__bemtvi_read",
                js(
                    &ctx,
                    Function::new(
                        ctx.clone(),
                        move |path: String| -> rquickjs::Result<String> {
                            read_in_tree(&read_root, Path::new(&path)).map_err(|e| {
                                rquickjs::Error::new_from_js_message(
                                    "path",
                                    "string",
                                    e.to_string(),
                                )
                            })
                        },
                    ),
                )?,
            ),
        )?;
        js(
            &ctx,
            globals.set(
                "__bemtvi_dirname",
                js(
                    &ctx,
                    Function::new(ctx.clone(), |path: String| -> String {
                        Path::new(&path)
                            .parent()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    }),
                )?,
            ),
        )?;
        js(&ctx, ctx.eval::<(), _>(REQUIRE_PRELUDE))?;

        let promise = js(&ctx, Module::evaluate(ctx.clone(), "dsl", DSL))?;
        js(&ctx, promise.finish::<()>())?;
        let output: Option<String> = js(&ctx, ctx.eval("globalThis.output"))?;
        let output = output.with_context(|| {
            format!("evaluating {} left no grammar output", grammar_js.display())
        })?;
        // Pretty-print it, as `tree-sitter generate` writes `src/grammar.json`.
        let parsed: serde_json::Value = serde_json::from_str(&output)
            .with_context(|| format!("{} produced invalid JSON", grammar_js.display()))?;
        // The DSL always sets `output`, wrapping whatever the grammar exported (nothing
        // at all gives just `{"$schema": …}`), so a missing grammar shows as no `name`.
        if parsed.get("name").is_none() {
            bail!(
                "{} defined no grammar (expected `module.exports = grammar({{…}})` or an \
                 ES module exporting one)",
                grammar_js.display()
            );
        }
        Ok(serde_json::to_string_pretty(&parsed)?)
    });
    if timed_out.load(std::sync::atomic::Ordering::Relaxed) {
        bail!(
            "{} took longer than {:?} to evaluate; stopped",
            grammar_js.display(),
            timeout
        );
    }
    result.with_context(|| format!("evaluate {}", grammar_js.display()))
}

/// `r`, with a QuickJS error turned into an `anyhow` one ([`js_error`]).
fn js<T>(ctx: &Ctx<'_>, r: rquickjs::Result<T>) -> Result<T> {
    r.map_err(|e| js_error(ctx, e))
}

/// A QuickJS error as an `anyhow` error. A thrown JS exception carries its message
/// and stack; anything else is rquickjs's own description.
fn js_error(ctx: &Ctx<'_>, err: rquickjs::Error) -> anyhow::Error {
    if !matches!(err, rquickjs::Error::Exception) {
        return anyhow::anyhow!("{err}");
    }
    let thrown: Value = ctx.catch();
    if let Some(exc) = thrown.as_exception() {
        let message = exc.message().unwrap_or_default();
        return match exc.stack() {
            Some(stack) if !stack.trim().is_empty() => {
                anyhow::anyhow!("{message}\n{}", stack.trim_end())
            }
            _ => anyhow::anyhow!("{message}"),
        };
    }
    match thrown.get::<String>() {
        Ok(s) => anyhow::anyhow!("uncaught: {s}"),
        Err(_) => anyhow::anyhow!("uncaught JS exception"),
    }
}

/// Resolve the module `name` imported or required from the file `from`, the way node
/// does for the cases grammars use: a relative or absolute path tries the name as-is,
/// then with `.js`, `.mjs`, `.json`, then as a directory's `index.js`; a bare name is
/// looked up in each `node_modules` from `from`'s directory up to `root`. `None` when
/// nothing matches or the match lies outside `root`.
fn resolve_module(root: &Path, from: &Path, name: &str) -> Option<PathBuf> {
    let from_dir = if from.is_file() {
        from.parent()?.to_path_buf()
    } else {
        root.to_path_buf()
    };
    let is_path = name.starts_with("./")
        || name.starts_with("../")
        || name.starts_with(".\\")
        || name.starts_with("..\\")
        || Path::new(name).is_absolute();
    let bases: Vec<PathBuf> = if is_path {
        vec![normalize(&from_dir.join(name))]
    } else {
        let mut dirs = Vec::new();
        let mut dir = Some(from_dir.as_path());
        while let Some(d) = dir {
            if !d.starts_with(root) {
                break;
            }
            dirs.push(normalize(&d.join("node_modules").join(name)));
            dir = d.parent();
        }
        dirs
    };
    for base in bases {
        if !base.starts_with(root) {
            continue;
        }
        let mut candidates = vec![base.clone()];
        for ext in ["js", "mjs", "cjs", "json"] {
            let mut s = base.clone().into_os_string();
            s.push(".");
            s.push(ext);
            candidates.push(PathBuf::from(s));
        }
        candidates.push(base.join("index.js"));
        if let Some(hit) = candidates.into_iter().find(|c| c.is_file()) {
            // Symlinks in the tarball must not lead out of the tree.
            let real = std::fs::canonicalize(&hit).ok()?;
            return dunce(real).starts_with(root).then_some(hit);
        }
    }
    None
}

/// Read `path` as UTF-8, refusing anything outside `root`.
fn read_in_tree(root: &Path, path: &Path) -> Result<String> {
    let real = canonical(path)?;
    if !real.starts_with(root) {
        bail!("{} is outside the grammar's source tree", path.display());
    }
    std::fs::read_to_string(&real).with_context(|| format!("read {}", path.display()))
}

/// Resolves ES module imports (`import … from`, and the DSL's `import()` of the
/// grammar) within the source tree.
struct TreeResolver {
    root: PathBuf,
}

impl Resolver for TreeResolver {
    fn resolve<'js>(
        &mut self,
        _ctx: &Ctx<'js>,
        base: &str,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<String> {
        resolve_module(&self.root, Path::new(base), name)
            .and_then(|p| path_str(&p).ok())
            .ok_or_else(|| {
                rquickjs::Error::new_resolving_message(
                    base,
                    name,
                    "not found in the grammar's source tree (node built-ins and \
                     node_modules packages are not available)",
                )
            })
    }
}

/// Loads a resolved ES module from disk; a `.json` module default-exports its value.
struct TreeLoader {
    root: PathBuf,
}

impl Loader for TreeLoader {
    fn load<'js>(
        &mut self,
        ctx: &Ctx<'js>,
        name: &str,
        _attributes: Option<ImportAttributes<'js>>,
    ) -> rquickjs::Result<Module<'js, rquickjs::module::Declared>> {
        let source = read_in_tree(&self.root, Path::new(name))
            .map_err(|e| rquickjs::Error::new_loading_message(name, e.to_string()))?;
        let source = if name.ends_with(".json") {
            let literal = serde_json::to_string(&source)
                .map_err(|e| rquickjs::Error::new_loading_message(name, e.to_string()))?;
            format!("export default JSON.parse({literal});")
        } else {
            source
        };
        Module::declare(ctx.clone(), name, source)
    }
}

/// `path` made absolute with symlinks resolved, without Windows' `\\?\` prefix
/// (which the `starts_with` checks and QuickJS module names don't expect).
fn canonical(path: &Path) -> Result<PathBuf> {
    let real =
        std::fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))?;
    Ok(dunce(real))
}

fn dunce(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if !rest.starts_with("UNC\\") {
                return PathBuf::from(rest.to_string());
            }
        }
    }
    path
}

/// Fold `.` and `..` out of `path` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn path_str(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))
}
