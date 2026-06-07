//! The `vim.treesitter` Lua platform's low-level primitives — nxvim's analogue
//! of neovim's `src/nvim/lua/treesitter.c`. This is the only bespoke code under
//! the platform: it exposes the loaded grammars to Lua as `TSParser` / `TSTree`
//! / `TSNode` / `TSQuery` userdata, over which neovim's *own* high-level
//! treesitter Lua (`vim/treesitter/*.lua`, vendored in a later phase) runs
//! unmodified. See `docs/specs/2026-06-07-vim-treesitter-lua-platform.md`.
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
use std::path::Path;
use std::rc::Rc;

use mlua::{Lua, MetaMethod, Table, UserData, UserDataMethods, UserDataRef, Value, Variadic};
use tree_sitter::{InputEdit, Node, Parser, Point, Query, Range, Tree};

use crate::loader::{LoadError, LoadedLanguage};

/// Register the `vim._create_ts_parser` / `vim._ts_has_language` /
/// `vim._ts_parse_query` primitives (and their userdata types) onto the shared
/// `vim` table. Called once during runtime construction, after the rest of the
/// `vim.*` bridge exists. `data_dir` is where grammars are loaded from (the same
/// `parser/` tree the highlight [`crate::Engine`] uses).
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
    // grammar. Iteration over its captures (the query cursor consumed by the
    // vendored `query.lua`) is a later phase; this phase proves compilation and
    // the loud error path on a malformed query.
    let dd = data_dir.to_path_buf();
    vim.set(
        "_ts_parse_query",
        lua.create_function(move |_, (lang, src): (String, String)| {
            let language = load_language(&dd, &lang)?;
            let query = Query::new(&language.language, &src).map_err(|e| {
                mlua::Error::RuntimeError(format!("invalid treesitter query for '{lang}': {e}"))
            })?;
            Ok(LuaQuery {
                query,
                _language: language,
            })
        })?,
    )?;

    Ok(())
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
        // `parser:parse(old_tree|nil, source, include_bytes?)` -> tree, ranges.
        // `source` is a string in this phase (the snapshot/buffer-handle form is
        // a later phase); `old_tree` enables incremental reparse. Returns the new
        // tree and the changed ranges (vs the old tree, or the included ranges of
        // a fresh parse), matching neovim's `parser_parse`.
        methods.add_method_mut(
            "parse",
            |lua, this, (old, source, include_bytes): (Option<UserDataRef<LuaTree>>, Value, Option<bool>)| {
                let include_bytes = include_bytes.unwrap_or(false);
                let text = match source {
                    Value::String(s) => s.as_bytes().to_vec(),
                    Value::Integer(_) | Value::Number(_) => {
                        return Err(mlua::Error::RuntimeError(
                            "TSParser:parse with a buffer handle is not supported yet; pass the source as a string"
                                .into(),
                        ))
                    }
                    other => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "TSParser:parse expected a string source, got {}",
                            other.type_name()
                        )))
                    }
                };

                let old_tree = old.as_ref().map(|t| &t.inner.tree);
                let new_tree = this.parser.parse(text.as_slice(), old_tree).ok_or_else(|| {
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
    }
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

/// `TSQuery` userdata: a compiled query. Capture iteration (the cursor the
/// vendored `query.lua` drives) lands in a later phase; for now it carries the
/// compiled query and proves the compile + error paths.
struct LuaQuery {
    #[allow(dead_code)]
    query: Query,
    // Kept alive so the query's capture/predicate tables outlive it.
    _language: Rc<LoadedLanguage>,
}

impl UserData for LuaQuery {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, _, ()| Ok("<query>".to_string()));
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
