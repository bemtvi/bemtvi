//! The `vim.treesitter` Lua platform's low-level primitives — nxvim's analogue
//! of neovim's `src/nvim/lua/treesitter.c`. This is the only bespoke code under
//! the platform: it exposes the loaded grammars to Lua as `TSParser` / `TSTree`
//! / `TSNode` / `TSQuery` / `TSQueryCursor` userdata, over which neovim's *own*
//! high-level treesitter Lua (`vim/treesitter/*.lua`, vendored under
//! `nxvim-lua/src/vendor/nvim/` and wired in
//! `nxvim-lua/src/prelude/treesitter.lua`) runs unmodified. See
//! `docs/specs/2026-06-07-vim-treesitter-lua-platform.md`.
//!
//! The query cursor ([`LuaQueryCursor`]) and `TSQuery:inspect` are ported over the
//! raw `tree_sitter::ffi` so matches are returned **unfiltered** — predicate
//! evaluation lives in the vendored `query.lua`, bug-for-bug with upstream (the
//! safe Rust iterator's text-predicate filtering would diverge on `#match?`).
//!
//! ## The lifetime model (the crux)
//!
//! tree-sitter's `Node<'tree>` borrows its `Tree`, but mlua userdata must be
//! `'static`. We reconcile this the way established bindings do: a [`TreeInner`]
//! (the `Tree` plus the [`LoadedLanguage`] whose code it points into) lives
//! behind an [`Rc`], every [`LuaNode`] co-owns that `Rc`, and the node's borrow
//! is erased to `'static`. This is sound because:
//!
//! - **Trees are immutable snapshots.** A `TSTree` userdata is never edited in
//!   place — `tree:edit()` clones, edits the clone, and returns a *new* tree
//!   (matching neovim). So any outstanding node keeps pointing at a still-valid,
//!   unchanged tree.
//! - **Nodes co-own their tree *and* its grammar.** Deriving a child/parent/
//!   sibling clones the `Rc<TreeInner>`, so a node handed to Lua can never
//!   outlive either its tree or the dynamic library the tree's node types live
//!   in, even after the parser moves on.
//! - **Single-threaded.** The runtime is already `Rc<RefCell<…>>` / non-`Send`;
//!   no `Send`/`Sync` is introduced.

use std::cell::Cell;
use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::path::Path;
use std::rc::Rc;
use std::slice;

use mlua::{Lua, MetaMethod, Table, UserData, UserDataMethods, UserDataRef, Value, Variadic};
use tree_sitter::{ffi, InputEdit, Language, Node, Parser, Point, Query, Range, Tree};

use crate::loader::{LoadError, LoadedLanguage};

/// Register the `vim._create_ts_parser` / `vim._ts_has_language` /
/// `vim._ts_parse_query` / `vim._ts_{get_language_version,get_minimum_language_version,
/// inspect_language}` / `vim._create_ts_querycursor` primitives (and their
/// userdata types) onto the shared `vim` table. Called once during runtime
/// construction, after the rest of the `vim.*` bridge exists. `data_dir` is where
/// grammars are loaded from (the same `parser/` tree the highlight
/// [`crate::Engine`] uses).
pub fn install(lua: &Lua, data_dir: &Path) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;

    // `vim._create_ts_parser(lang)` — a fresh parser bound to `lang`'s grammar.
    // `lang` is a name (`"rust"`), resolved through the loader's on-disk layout
    // rather than neovim's registered-language map; that layout *is* our
    // registry. Fails loud if no parser is installed or the grammar is broken.
    let dd = data_dir.to_path_buf();
    vim.set(
        "_create_ts_parser",
        lua.create_function(move |_, lang: String| {
            let language = load_language(&dd, &lang)?;
            let mut parser = Parser::new();
            parser.set_language(&language.language).map_err(|e| {
                mlua::Error::RuntimeError(format!("treesitter language '{lang}' incompatible: {e}"))
            })?;
            Ok(LuaParser { parser, language })
        })?,
    )?;

    // `vim._ts_has_language(lang)` — whether a usable parser is installed.
    let dd = data_dir.to_path_buf();
    vim.set(
        "_ts_has_language",
        lua.create_function(move |_, lang: String| Ok(LoadedLanguage::load(&dd, &lang).is_ok()))?,
    )?;

    // `vim._ts_parse_query(lang, src)` — compile a query string against `lang`'s
    // grammar into a `TSQuery` userdata. Built over the *raw* `TSQuery` pointer
    // (not the safe `Query` wrapper) so the query cursor can iterate it
    // **unfiltered** and `:inspect()` can read the raw predicate steps — exactly
    // as neovim's `treesitter.c` does (predicate evaluation lives in `query.lua`,
    // not in tree-sitter). Fails loud on a malformed query.
    let dd = data_dir.to_path_buf();
    vim.set(
        "_ts_parse_query",
        lua.create_function(move |_, (lang, src): (String, String)| {
            let language = load_language(&dd, &lang)?;
            let ptr = Query::new_raw(&language.language, &src).map_err(|e| {
                mlua::Error::RuntimeError(format!("invalid treesitter query for '{lang}': {e}"))
            })?;
            Ok(LuaQuery {
                inner: Rc::new(QueryInner {
                    query: ptr,
                    _language: language,
                }),
            })
        })?,
    )?;

    // `vim._ts_get_language_version()` / `vim._ts_get_minimum_language_version()`
    // — the tree-sitter library's ABI version and the oldest it can load,
    // surfaced by `vim.treesitter.{language_version,minimum_language_version}`.
    vim.set(
        "_ts_get_language_version",
        lua.create_function(|_, ()| Ok(tree_sitter::LANGUAGE_VERSION as i64))?,
    )?;
    vim.set(
        "_ts_get_minimum_language_version",
        lua.create_function(|_, ()| Ok(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION as i64))?,
    )?;

    // `vim._ts_inspect_language(lang)` — symbols/fields/ABI for `lang`'s grammar,
    // the analogue of neovim's `tslua_inspect_lang`, backing
    // `vim.treesitter.language.inspect()`.
    let dd = data_dir.to_path_buf();
    vim.set(
        "_ts_inspect_language",
        lua.create_function(move |lua, lang: String| {
            let language = load_language(&dd, &lang)?;
            inspect_language(lua, &language.language)
        })?,
    )?;

    // `vim._create_ts_querycursor(node, query, opts)` — a stateful cursor that
    // walks `query`'s matches/captures under `node`, the engine `query.lua`'s
    // `iter_captures`/`iter_matches` drive. Ported from neovim's `treesitter.c`
    // over the raw tree-sitter cursor FFI, so matches are returned **unfiltered**
    // (predicates are evaluated in Lua, matching upstream bug-for-bug).
    vim.set(
        "_create_ts_querycursor",
        lua.create_function(
            |_, (node, query, opts): (UserDataRef<LuaNode>, UserDataRef<LuaQuery>, Table)| {
                create_querycursor(&node, &query, &opts)
            },
        )?,
    )?;

    Ok(())
}

/// Read `opts.<key>` as a `u32`, treating absent/`nil` as `default` and clamping
/// a negative number to `0` (lua51 hands every number across as a float, so the
/// cursor range fields arrive as `Value::Number`).
fn opt_u32(opts: &Table, key: &str, default: u32) -> mlua::Result<u32> {
    match opts.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Integer(n) => Ok(n.max(0) as u32),
        Value::Number(n) => Ok(if n < 0.0 { 0 } else { n as u32 }),
        other => Err(mlua::Error::RuntimeError(format!(
            "querycursor opt '{key}' must be a number, got {}",
            other.type_name()
        ))),
    }
}

/// Optional `u32` cursor option — `None` when absent/`nil`.
fn opt_u32_opt(opts: &Table, key: &str) -> mlua::Result<Option<u32>> {
    match opts.get::<Value>(key)? {
        Value::Nil => Ok(None),
        _ => Ok(Some(opt_u32(opts, key, 0)?)),
    }
}

/// Build and exec a [`LuaQueryCursor`] over `node` for `query`, applying the
/// point range / depth / match-limit options. Mirrors `treesitter.c`'s
/// `query_next_metadata`/cursor setup.
fn create_querycursor(
    node: &LuaNode,
    query: &LuaQuery,
    opts: &Table,
) -> mlua::Result<LuaQueryCursor> {
    let start = Point::new(
        opt_u32(opts, "start_row", 0)? as usize,
        opt_u32(opts, "start_col", 0)? as usize,
    );
    let end = Point::new(
        opt_u32(opts, "end_row", 0)? as usize,
        opt_u32(opts, "end_col", 0)? as usize,
    );
    let max_start_depth = opt_u32_opt(opts, "max_start_depth")?;
    let match_limit = opt_u32_opt(opts, "match_limit")?;

    // SAFETY: `cursor` is a fresh tree-sitter cursor; `query.inner` keeps the
    // `TSQuery` alive (co-owned `Rc`), and `node.inner` keeps the searched tree
    // alive — both stored in the returned `LuaQueryCursor`. The node's raw
    // `TSNode` is valid for the exec call because its tree is alive here.
    let cursor = unsafe {
        let cursor = ffi::ts_query_cursor_new();
        ffi::ts_query_cursor_set_point_range(
            cursor,
            ffi::TSPoint {
                row: start.row as u32,
                column: start.column as u32,
            },
            ffi::TSPoint {
                row: end.row as u32,
                column: end.column as u32,
            },
        );
        if let Some(d) = max_start_depth {
            ffi::ts_query_cursor_set_max_start_depth(cursor, d);
        }
        if let Some(l) = match_limit {
            ffi::ts_query_cursor_set_match_limit(cursor, l);
        }
        ffi::ts_query_cursor_exec(cursor, query.inner.query, node.node.into_raw());
        cursor
    };

    Ok(LuaQueryCursor {
        cursor,
        inner: node.inner.clone(),
        _query: query.inner.clone(),
    })
}

/// Project a grammar's symbols/fields/ABI into the table neovim's
/// `tslua_inspect_lang` returns (`{ symbols, fields, abi_version, state_count }`).
fn inspect_language(lua: &Lua, language: &Language) -> mlua::Result<Table> {
    // Borrow the raw `TSLanguage`: clone (incr refcount) → `into_raw` (no decr) →
    // `from_raw`'s drop (decr) balances it, so no leak and no extra ownership.
    let raw = language.clone().into_raw();
    let out = lua.create_table()?;
    // SAFETY: `raw` points at a live `TSLanguage` for this scope; reclaimed by
    // the `from_raw` below.
    let result = (|| -> mlua::Result<()> {
        unsafe {
            let symbols = lua.create_table()?;
            let nsymbols = ffi::ts_language_symbol_count(raw);
            for i in 0..nsymbols {
                let t = ffi::ts_language_symbol_type(raw, i as ffi::TSSymbol);
                if t == ffi::TSSymbolTypeAuxiliary {
                    continue;
                }
                let name = CStr::from_ptr(ffi::ts_language_symbol_name(raw, i as ffi::TSSymbol))
                    .to_string_lossy();
                let named = t != ffi::TSSymbolTypeAnonymous;
                let key = if named {
                    name.into_owned()
                } else {
                    format!("\"{name}\"")
                };
                symbols.set(key, named)?;
            }
            out.set("symbols", symbols)?;

            let fields = lua.create_table()?;
            let nfields = ffi::ts_language_field_count(raw);
            // Field IDs run 1..=nfields (id 0 maps to NULL).
            for i in 1..=nfields {
                let p = ffi::ts_language_field_name_for_id(raw, i as ffi::TSFieldId);
                if !p.is_null() {
                    fields.set(i as i64, CStr::from_ptr(p).to_string_lossy().into_owned())?;
                }
            }
            out.set("fields", fields)?;

            out.set("abi_version", ffi::ts_language_abi_version(raw) as i64)?;
            out.set("state_count", ffi::ts_language_state_count(raw) as i64)?;
            out.set("_wasm", false)?;
            Ok(())
        }
    })();
    // SAFETY: balances the `clone().into_raw()` above.
    drop(unsafe { Language::from_raw(raw) });
    result?;
    Ok(out)
}

/// Load just the `Language` for `lang`, mapping the loader's missing-vs-broken
/// distinction onto loud Lua errors (a plugin asking for a parser it doesn't
/// have should hear about it, not get a silent `nil`).
fn load_language(data_dir: &Path, lang: &str) -> mlua::Result<Rc<LoadedLanguage>> {
    LoadedLanguage::load(data_dir, lang)
        .map(Rc::new)
        .map_err(|e| {
            mlua::Error::RuntimeError(match e {
                LoadError::NotInstalled => {
                    format!("no treesitter parser installed for language '{lang}'")
                }
                LoadError::Failed(err) => {
                    format!("loading treesitter parser for '{lang}': {err:#}")
                }
            })
        })
}

/// A `Tree` and the grammar library its nodes point into, co-owned by every
/// node/tree userdata so neither can be dropped while a node is live. Held
/// behind an `Rc`; never mutated in place (see the module's lifetime model).
struct TreeInner {
    tree: Tree,
    // Kept alive so the dynamic library backing `tree`'s node-type strings and
    // language tables outlives every node derived from it.
    _language: Rc<LoadedLanguage>,
}

/// Erase a freshly-derived root node's borrow to `'static`. Sound because the
/// caller pairs it with a clone of the same `Rc<TreeInner>`, which keeps the
/// borrowed tree alive and immutable for the node's whole life.
fn root_static(inner: &Rc<TreeInner>) -> Node<'static> {
    // SAFETY: `Node` is `#[repr(transparent)]` over `(TSNode, PhantomData)`, so
    // the transmute only rewrites the phantom lifetime. The node borrows
    // `inner.tree`; `inner` (an `Rc`) is co-stored in the resulting `LuaNode`,
    // keeping that tree alive and unmutated.
    unsafe { std::mem::transmute::<Node<'_>, Node<'static>>(inner.tree.root_node()) }
}

// ----- TSParser -------------------------------------------------------------

/// `TSParser` userdata: a tree-sitter `Parser` bound to one grammar.
struct LuaParser {
    parser: Parser,
    language: Rc<LoadedLanguage>,
}

impl UserData for LuaParser {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `parser:parse(old_tree|nil, source, include_bytes?, timeout?)` -> tree,
        // ranges. `source` is either a literal string (`get_string_parser`) or a
        // **buffer handle** (an integer bufnr), in which case the text is read
        // from the pushed snapshot (`vim._bufs[bufnr].lines`) — nxvim's analogue
        // of neovim's read-from-buffer callback. `old_tree` enables incremental
        // reparse. `timeout` (neovim's coroutine-yield budget) is accepted and
        // ignored: nxvim parses synchronously to completion. Returns the new tree
        // and the changed ranges (vs the old tree, or the included ranges of a
        // fresh parse), matching neovim's `parser_parse`.
        methods.add_method_mut(
            "parse",
            |lua,
             this,
             (old, source, include_bytes, _timeout): (
                Option<UserDataRef<LuaTree>>,
                Value,
                Option<bool>,
                Option<Value>,
            )| {
                let include_bytes = include_bytes.unwrap_or(false);
                let text = read_source(lua, &source)?;

                let old_tree = old.as_ref().map(|t| &t.inner.tree);
                let new_tree = this
                    .parser
                    .parse(text.as_slice(), old_tree)
                    .ok_or_else(|| {
                        mlua::Error::RuntimeError(
                            "treesitter parse failed (no language set or incompatible ABI)".into(),
                        )
                    })?;

                let ranges: Vec<Range> = match old_tree {
                    Some(old) => old.changed_ranges(&new_tree).collect(),
                    None => new_tree.included_ranges(),
                };
                let ranges_tbl = ranges_to_table(lua, &ranges, include_bytes)?;

                let tree = LuaTree {
                    inner: Rc::new(TreeInner {
                        tree: new_tree,
                        _language: this.language.clone(),
                    }),
                };
                Ok((tree, ranges_tbl))
            },
        );

        // `parser:reset()` — drop the cached tree so the next parse is fresh.
        methods.add_method_mut("reset", |_, this, ()| {
            this.parser.reset();
            Ok(())
        });

        // `parser:set_included_ranges(ranges)` — restrict the next parse to the
        // given regions (used for injections). Each range is the `Range6` list
        // `{start_row, start_col, start_byte, end_row, end_col, end_byte}` neovim
        // produces; an empty `ranges` clears the restriction (parse the whole
        // document), matching tree-sitter's default.
        methods.add_method_mut("set_included_ranges", |_, this, ranges: Vec<Table>| {
            let mut out = Vec::with_capacity(ranges.len());
            for r in ranges {
                out.push(Range {
                    start_point: Point::new(r.get(1)?, r.get(2)?),
                    start_byte: r.get(3)?,
                    end_point: Point::new(r.get(4)?, r.get(5)?),
                    end_byte: r.get(6)?,
                });
            }
            this.parser
                .set_included_ranges(&out)
                .map_err(|e| mlua::Error::RuntimeError(format!("invalid included ranges: {e}")))?;
            Ok(())
        });
    }
}

/// Materialize a `TSParser:parse` source into bytes. A string is taken verbatim;
/// an integer/number is a buffer handle, read from the pushed snapshot
/// (`vim._bufs[bufnr].lines`, joined with `\n`). Anything else, or a buffer with
/// no snapshot, is a loud error (per the no-silent-stub rule).
fn read_source(lua: &Lua, source: &Value) -> mlua::Result<Vec<u8>> {
    match source {
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        Value::Integer(n) => read_buf_snapshot(lua, *n),
        Value::Number(n) => read_buf_snapshot(lua, *n as i64),
        other => Err(mlua::Error::RuntimeError(format!(
            "TSParser:parse expected a string or buffer handle, got {}",
            other.type_name()
        ))),
    }
}

/// Read buffer `bufnr`'s snapshot lines and join them with `\n`. The snapshot is
/// the `vim._bufs` mirror the server pushes before any Lua runs.
fn read_buf_snapshot(lua: &Lua, bufnr: i64) -> mlua::Result<Vec<u8>> {
    let vim: Table = lua.globals().get("vim")?;
    let bufs: Table = vim.get("_bufs")?;
    let buf: Value = bufs.get(bufnr)?;
    let Value::Table(buf) = buf else {
        return Err(mlua::Error::RuntimeError(format!(
            "TSParser:parse: no snapshot for buffer {bufnr} (is it loaded?)"
        )));
    };
    let lines: Table = buf.get("lines")?;
    let mut text = Vec::new();
    for (i, line) in lines.sequence_values::<mlua::String>().enumerate() {
        if i > 0 {
            text.push(b'\n');
        }
        text.extend_from_slice(&line?.as_bytes());
    }
    Ok(text)
}

// ----- TSTree ---------------------------------------------------------------

/// `TSTree` userdata: an immutable parse-tree snapshot.
#[derive(Clone)]
struct LuaTree {
    inner: Rc<TreeInner>,
}

impl UserData for LuaTree {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("root", |_, this, ()| {
            Ok(LuaNode {
                inner: this.inner.clone(),
                node: root_static(&this.inner),
            })
        });

        methods.add_method("copy", |_, this, ()| Ok(this.clone()));

        // `tree:edit(...)` — clone-and-edit, returning a *new* tree (the tree is
        // never mutated in place; see the lifetime model). Args are the nine
        // fields of a `TSInputEdit`, byte offsets then point pairs.
        methods.add_method(
            "edit",
            |_,
             this,
             (
                start_byte,
                old_end_byte,
                new_end_byte,
                start_row,
                start_col,
                old_end_row,
                old_end_col,
                new_end_row,
                new_end_col,
            ): (
                usize,
                usize,
                usize,
                usize,
                usize,
                usize,
                usize,
                usize,
                usize,
            )| {
                let edit = InputEdit {
                    start_byte,
                    old_end_byte,
                    new_end_byte,
                    start_position: Point::new(start_row, start_col),
                    old_end_position: Point::new(old_end_row, old_end_col),
                    new_end_position: Point::new(new_end_row, new_end_col),
                };
                let mut tree = this.inner.tree.clone();
                tree.edit(&edit);
                Ok(LuaTree {
                    inner: Rc::new(TreeInner {
                        tree,
                        _language: this.inner._language.clone(),
                    }),
                })
            },
        );

        methods.add_method(
            "included_ranges",
            |lua, this, include_bytes: Option<bool>| {
                ranges_to_table(
                    lua,
                    &this.inner.tree.included_ranges(),
                    include_bytes.unwrap_or(false),
                )
            },
        );

        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<tree>".to_string()));
    }
}

// ----- TSNode ---------------------------------------------------------------

/// `TSNode` userdata: a node within a [`LuaTree`], co-owning the tree it borrows.
#[derive(Clone)]
struct LuaNode {
    inner: Rc<TreeInner>,
    node: Node<'static>,
}

impl LuaNode {
    /// Wrap a node derived from `self` (same tree, already `'static`) as userdata.
    fn child(&self, node: Node<'static>) -> LuaNode {
        LuaNode {
            inner: self.inner.clone(),
            node,
        }
    }

    /// Wrap an optional derived node — `nil` when absent (parent of the root,
    /// child past the end, …), matching neovim's `push_node`.
    fn child_opt(&self, node: Option<Node<'static>>) -> Option<LuaNode> {
        node.map(|n| self.child(n))
    }
}

impl UserData for LuaNode {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("type", |_, this, ()| Ok(this.node.kind().to_string()));
        methods.add_method("symbol", |_, this, ()| Ok(this.node.kind_id()));
        methods.add_method("id", |_, this, ()| Ok(this.node.id() as i64));

        // `node:range(include_bytes?)` -> start_row, start_col, [start_byte],
        // end_row, end_col, [end_byte].
        methods.add_method("range", |_, this, include_bytes: Option<bool>| {
            let (s, e) = (this.node.start_position(), this.node.end_position());
            let mut v: Vec<i64> = Vec::with_capacity(6);
            v.push(s.row as i64);
            v.push(s.column as i64);
            if include_bytes.unwrap_or(false) {
                v.push(this.node.start_byte() as i64);
            }
            v.push(e.row as i64);
            v.push(e.column as i64);
            if include_bytes.unwrap_or(false) {
                v.push(this.node.end_byte() as i64);
            }
            Ok(Variadic::from_iter(v))
        });

        methods.add_method("start", |_, this, ()| {
            let s = this.node.start_position();
            Ok((s.row as i64, s.column as i64, this.node.start_byte() as i64))
        });
        methods.add_method("end_", |_, this, ()| {
            let e = this.node.end_position();
            Ok((e.row as i64, e.column as i64, this.node.end_byte() as i64))
        });
        methods.add_method("byte_length", |_, this, ()| {
            Ok((this.node.end_byte() - this.node.start_byte()) as i64)
        });

        methods.add_method("named", |_, this, ()| Ok(this.node.is_named()));
        methods.add_method("missing", |_, this, ()| Ok(this.node.is_missing()));
        methods.add_method("extra", |_, this, ()| Ok(this.node.is_extra()));
        methods.add_method("has_error", |_, this, ()| Ok(this.node.has_error()));
        methods.add_method("has_changes", |_, this, ()| Ok(this.node.has_changes()));
        methods.add_method("sexpr", |_, this, ()| Ok(this.node.to_sexp()));

        methods.add_method("child_count", |_, this, ()| {
            Ok(this.node.child_count() as i64)
        });
        methods.add_method("named_child_count", |_, this, ()| {
            Ok(this.node.named_child_count() as i64)
        });
        methods.add_method("child", |_, this, i: u32| {
            Ok(this.child_opt(this.node.child(i)))
        });
        methods.add_method("named_child", |_, this, i: u32| {
            Ok(this.child_opt(this.node.named_child(i)))
        });

        methods.add_method("parent", |_, this, ()| {
            Ok(this.child_opt(this.node.parent()))
        });
        methods.add_method("next_sibling", |_, this, ()| {
            Ok(this.child_opt(this.node.next_sibling()))
        });
        methods.add_method("prev_sibling", |_, this, ()| {
            Ok(this.child_opt(this.node.prev_sibling()))
        });
        methods.add_method("next_named_sibling", |_, this, ()| {
            Ok(this.child_opt(this.node.next_named_sibling()))
        });
        methods.add_method("prev_named_sibling", |_, this, ()| {
            Ok(this.child_opt(this.node.prev_named_sibling()))
        });

        methods.add_method(
            "descendant_for_range",
            |_, this, (sr, sc, er, ec): (usize, usize, usize, usize)| {
                Ok(this.child_opt(
                    this.node
                        .descendant_for_point_range(Point::new(sr, sc), Point::new(er, ec)),
                ))
            },
        );
        methods.add_method(
            "named_descendant_for_range",
            |_, this, (sr, sc, er, ec): (usize, usize, usize, usize)| {
                Ok(this.child_opt(
                    this.node
                        .named_descendant_for_point_range(Point::new(sr, sc), Point::new(er, ec)),
                ))
            },
        );

        methods.add_method(
            "child_with_descendant",
            |_, this, descendant: UserDataRef<LuaNode>| {
                Ok(this.child_opt(this.node.child_with_descendant(descendant.node)))
            },
        );

        // `node:field(name)` -> list of child nodes carrying that field name.
        methods.add_method("field", |lua, this, name: String| {
            let t = lua.create_table()?;
            let mut j = 0;
            for i in 0..this.node.child_count() as u32 {
                if this.node.field_name_for_child(i) == Some(name.as_str()) {
                    if let Some(c) = this.node.child(i) {
                        j += 1;
                        t.set(j, this.child(c))?;
                    }
                }
            }
            Ok(t)
        });

        // `node:named_children()` -> list of the named children.
        methods.add_method("named_children", |lua, this, ()| {
            let t = lua.create_table()?;
            let mut j = 0;
            for i in 0..this.node.child_count() as u32 {
                if let Some(c) = this.node.child(i) {
                    if c.is_named() {
                        j += 1;
                        t.set(j, this.child(c))?;
                    }
                }
            }
            Ok(t)
        });

        // `node:iter_children()` -> stateful iterator yielding `child, field_name`.
        methods.add_method("iter_children", |lua, this, ()| {
            let node = this.clone();
            let i = Cell::new(0u32);
            lua.create_function(move |lua, ()| {
                let idx = i.get();
                if idx >= node.node.child_count() as u32 {
                    return Ok((None, Value::Nil));
                }
                i.set(idx + 1);
                let child = node.child(node.node.child(idx).expect("index in bounds"));
                let field = match node.node.field_name_for_child(idx) {
                    Some(name) => Value::String(lua.create_string(name)?),
                    None => Value::Nil,
                };
                Ok((Some(child), field))
            })
        });

        // `node:__has_ancestor(pred)` — does any ancestor's type appear in the
        // predicate's type list? `pred[1]`/`pred[2]` are the predicate name and
        // capture id; the candidate types start at `pred[3]` (matching neovim).
        methods.add_method("__has_ancestor", |_, this, pred: Table| {
            let mut types: Vec<String> = Vec::new();
            for i in 3..=pred.raw_len() {
                if let Ok(s) = pred.raw_get::<String>(i) {
                    types.push(s);
                }
            }
            let target = this.node.id();
            let mut cur = root_static(&this.inner);
            while cur.id() != target {
                if types.iter().any(|t| t == cur.kind()) {
                    return Ok(true);
                }
                match cur.child_with_descendant(this.node) {
                    Some(next) => cur = next,
                    None => break,
                }
            }
            Ok(false)
        });

        methods.add_method("root", |_, this, ()| {
            Ok(this.child(root_static(&this.inner)))
        });
        methods.add_method("tree", |_, this, ()| {
            Ok(LuaTree {
                inner: this.inner.clone(),
            })
        });

        methods.add_method("equal", |_, this, other: UserDataRef<LuaNode>| {
            Ok(this.node == other.node)
        });
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: UserDataRef<LuaNode>| {
            Ok(this.node == other.node)
        });
        methods.add_meta_method(MetaMethod::Len, |_, this, ()| {
            Ok(this.node.child_count() as i64)
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("<node {}>", this.node.kind()))
        });
    }
}

// ----- TSQuery --------------------------------------------------------------

/// A compiled `TSQuery` (raw pointer) and the grammar it was compiled against,
/// co-owned by every [`LuaQuery`] and any [`LuaQueryCursor`] iterating it, so the
/// query (and the dylib its strings live in) outlives every cursor. Held behind
/// an `Rc`; the `TSQuery` is freed when the last owner drops.
struct QueryInner {
    query: *mut ffi::TSQuery,
    // Kept alive so the query's capture/predicate string tables outlive it.
    _language: Rc<LoadedLanguage>,
}

impl Drop for QueryInner {
    fn drop(&mut self) {
        // SAFETY: `query` came from `Query::new_raw` and is owned solely by this
        // `QueryInner`; no cursor outlives the `Rc` (each co-owns it).
        unsafe { ffi::ts_query_delete(self.query) }
    }
}

/// `TSQuery` userdata: a compiled query the vendored `query.lua` introspects via
/// [`inspect`](LuaQuery) and iterates via [`LuaQueryCursor`].
#[derive(Clone)]
struct LuaQuery {
    inner: Rc<QueryInner>,
}

impl UserData for LuaQuery {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `query:inspect()` -> `{ captures = string[], patterns = table }`.
        // `captures[i+1]` is capture `i`'s name; `patterns[p+1]` is pattern `p`'s
        // list of predicate/directive specs, each a list whose first element is
        // the predicate name (string), capture args as `capture_id + 1` integers
        // and literal args as strings — the raw predicate-step grouping neovim's
        // `query_inspect` produces (predicates are evaluated later, in Lua).
        methods.add_method("inspect", |lua, this, ()| {
            query_inspect(lua, this.inner.query)
        });

        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<query>".to_string()));
    }
}

/// `slice::from_raw_parts` that tolerates the `(null, 0)` tree-sitter hands back
/// for an empty predicate/capture list — passing a null pointer to
/// `from_raw_parts` is UB even at length `0`.
///
/// # Safety
/// Same as [`slice::from_raw_parts`] when `len > 0`; otherwise a no-op.
unsafe fn raw_slice<'a, T>(ptr: *const T, len: usize) -> &'a [T] {
    if len == 0 {
        &[]
    } else {
        slice::from_raw_parts(ptr, len)
    }
}

/// Build the `{ captures, patterns }` table for `query`, porting neovim's
/// `query_inspect` over the raw tree-sitter predicate-step API.
fn query_inspect(lua: &Lua, query: *mut ffi::TSQuery) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    // SAFETY: `query` is a live `TSQuery` (co-owned by the caller's `LuaQuery`).
    unsafe {
        let patterns = lua.create_table()?;
        let npatterns = ffi::ts_query_pattern_count(query);
        for p in 0..npatterns {
            let mut count = 0u32;
            let steps_ptr = ffi::ts_query_predicates_for_pattern(query, p, &mut count);
            let steps = raw_slice(steps_ptr, count as usize);
            let pat = lua.create_table()?;
            let mut nextpred = 1i64;
            let mut pred = lua.create_table()?;
            let mut nextitem = 1i64;
            for step in steps {
                match step.type_ {
                    ffi::TSQueryPredicateStepTypeDone => {
                        pat.set(nextpred, pred)?;
                        nextpred += 1;
                        pred = lua.create_table()?;
                        nextitem = 1;
                    }
                    ffi::TSQueryPredicateStepTypeCapture => {
                        pred.set(nextitem, step.value_id as i64 + 1)?;
                        nextitem += 1;
                    }
                    ffi::TSQueryPredicateStepTypeString => {
                        let mut len = 0u32;
                        let sp = ffi::ts_query_string_value_for_id(query, step.value_id, &mut len);
                        let s = raw_slice(sp as *const u8, len as usize);
                        pred.set(nextitem, String::from_utf8_lossy(s).into_owned())?;
                        nextitem += 1;
                    }
                    _ => {}
                }
            }
            patterns.set(p as i64 + 1, pat)?;
        }
        out.set("patterns", patterns)?;

        let captures = lua.create_table()?;
        let ncaptures = ffi::ts_query_capture_count(query);
        for i in 0..ncaptures {
            let mut len = 0u32;
            let np = ffi::ts_query_capture_name_for_id(query, i, &mut len);
            let s = raw_slice(np as *const u8, len as usize);
            captures.set(i as i64 + 1, String::from_utf8_lossy(s).into_owned())?;
        }
        out.set("captures", captures)?;
    }
    Ok(out)
}

// ----- TSQueryCursor / TSQueryMatch -----------------------------------------

/// `TSQueryCursor` userdata: a stateful iterator over a query's matches/captures
/// under a node. Co-owns the searched tree and the query so both outlive it.
/// Ported from neovim's `treesitter.c` cursor.
struct LuaQueryCursor {
    cursor: *mut ffi::TSQueryCursor,
    // The searched tree — keeps the captured nodes' tree alive.
    inner: Rc<TreeInner>,
    // The query being iterated — keeps its `TSQuery` alive for the cursor.
    _query: Rc<QueryInner>,
}

impl Drop for LuaQueryCursor {
    fn drop(&mut self) {
        // SAFETY: `cursor` is owned solely by this struct.
        unsafe { ffi::ts_query_cursor_delete(self.cursor) }
    }
}

impl LuaQueryCursor {
    /// Snapshot a raw `TSQueryMatch` into a [`LuaMatch`], copying its captures out
    /// (the C `captures` pointer is only valid until the cursor next advances).
    fn snapshot_match(&self, m: &ffi::TSQueryMatch) -> LuaMatch {
        // SAFETY: `m.captures` points at `m.capture_count` valid captures for the
        // duration of this call; each node belongs to `self.inner`'s tree.
        let caps = unsafe { raw_slice(m.captures, m.capture_count as usize) };
        let captures = caps
            .iter()
            .map(|c| (c.index, unsafe { Node::from_raw(c.node) }))
            .collect();
        LuaMatch {
            inner: self.inner.clone(),
            id: m.id,
            pattern_index: m.pattern_index,
            captures,
        }
    }
}

impl UserData for LuaQueryCursor {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `cursor:next_capture()` -> (capture_id, node, match) | nil. The capture
        // id is `query.captures`-indexed (1-based); `match` exposes the whole
        // match. `nil` ends iteration.
        methods.add_method_mut("next_capture", |_, this, ()| {
            let mut m = MaybeUninit::<ffi::TSQueryMatch>::uninit();
            let mut capture_index = 0u32;
            // SAFETY: `this.cursor` is live; `m`/`capture_index` are written by the
            // call when it returns true.
            let got = unsafe {
                ffi::ts_query_cursor_next_capture(this.cursor, m.as_mut_ptr(), &mut capture_index)
            };
            if !got {
                return Ok((None, None, None));
            }
            let m = unsafe { m.assume_init() };
            let caps = unsafe { raw_slice(m.captures, m.capture_count as usize) };
            let cap = &caps[capture_index as usize];
            let node = LuaNode {
                inner: this.inner.clone(),
                node: unsafe { Node::from_raw(cap.node) },
            };
            Ok((
                Some(cap.index as i64 + 1),
                Some(node),
                Some(this.snapshot_match(&m)),
            ))
        });

        // `cursor:next_match()` -> match | nil.
        methods.add_method_mut("next_match", |_, this, ()| {
            let mut m = MaybeUninit::<ffi::TSQueryMatch>::uninit();
            // SAFETY: as above.
            let got = unsafe { ffi::ts_query_cursor_next_match(this.cursor, m.as_mut_ptr()) };
            if !got {
                return Ok(None);
            }
            let m = unsafe { m.assume_init() };
            Ok(Some(this.snapshot_match(&m)))
        });

        // `cursor:remove_match(match_id)` — drop a match the predicates rejected,
        // so its captures stop being yielded by `next_capture`.
        methods.add_method_mut("remove_match", |_, this, match_id: u32| {
            // SAFETY: `this.cursor` is live.
            unsafe { ffi::ts_query_cursor_remove_match(this.cursor, match_id) };
            Ok(())
        });
    }
}

/// `TSQueryMatch` userdata: one match's id, pattern, and captured nodes (snapshot
/// taken when the cursor produced it). Ported from neovim's `TSQueryMatch`.
struct LuaMatch {
    inner: Rc<TreeInner>,
    id: u32,
    pattern_index: u16,
    /// `(capture_index, node)` pairs; a capture index may repeat (quantifiers).
    captures: Vec<(u32, Node<'static>)>,
}

impl UserData for LuaMatch {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `match:info()` -> (match_id, pattern_id). `pattern_id` is 1-based (it
        // indexes `query.captures`/`_processed_patterns`); `match_id` is the raw
        // cursor id used with `remove_match`.
        methods.add_method("info", |_, this, ()| {
            Ok((this.id as i64, this.pattern_index as i64 + 1))
        });

        // `match:captures()` -> table<capture_id, TSNode[]>, capture ids 1-based.
        methods.add_method("captures", |lua, this, ()| {
            let out = lua.create_table()?;
            for (idx, node) in &this.captures {
                let key = *idx as i64 + 1;
                let list: Table = match out.get::<Value>(key)? {
                    Value::Table(t) => t,
                    _ => {
                        let t = lua.create_table()?;
                        out.set(key, &t)?;
                        t
                    }
                };
                list.set(
                    list.raw_len() + 1,
                    LuaNode {
                        inner: this.inner.clone(),
                        node: *node,
                    },
                )?;
            }
            Ok(out)
        });
    }
}

/// Project tree-sitter `Range`s into the Lua list-of-tuples shape neovim's
/// `push_ranges` produces: `{start_row, start_col, [start_byte], end_row,
/// end_col, [end_byte]}` per range.
fn ranges_to_table(lua: &Lua, ranges: &[Range], include_bytes: bool) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    for (i, r) in ranges.iter().enumerate() {
        let t = lua.create_table()?;
        let mut j = 0;
        let mut push = |v: usize| -> mlua::Result<()> {
            j += 1;
            t.set(j, v as i64)
        };
        push(r.start_point.row)?;
        push(r.start_point.column)?;
        if include_bytes {
            push(r.start_byte)?;
        }
        push(r.end_point.row)?;
        push(r.end_point.column)?;
        if include_bytes {
            push(r.end_byte)?;
        }
        out.set(i + 1, t)?;
    }
    Ok(out)
}
