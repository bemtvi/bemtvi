--------------------------------------------------------------------------------
-- Fragment highlighting — colouring code that isn't a whole program.
--
-- Run:
--   NXVIM_CONFIG=examples/fragment-highlighting \
--     cargo run -p nxvim -- examples/fragment-highlighting/sample.txt
--
-- Needs the `rust` and `python` grammars installed (`:TSInstall rust python`) —
-- the doc blocks below are rust and python, and with no grammar for a block's
-- language it simply renders plain.
--
-- Background: docs/plans/2026-08-06-fragment-highlighting.md.
--
-- The code blocks an LSP puts in a hover or a completion doc are not programs.
-- They are fragments (a struct field, a bare statement) or an annotation dialect
-- the server invented for display. This config fakes those replies with a
-- completion source so you can see all three outcomes without a language server.
--
-- Type `fie`, `let`, `sig`, `dia`, `pyd`, `pyc`, `pym` or `pyo` in insert mode;
-- `<C-n>` selects a row and the documentation float opens beside the popup.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 1. A FRAGMENT gets its real structure back.
--
-- `field: Vec<String>` is not a rust program — parsed as one it lands in error
-- recovery, which invents a construct and paints `Vec` as a @constructor. The
-- shipped framing for rust wraps it in `struct __nx { ... }`, which *is* a
-- program, so the colours come from a real parse.
--
-- Type-this:  i f i e <C-n>
-- See-that:   in the float, `Vec` and `String` are TYPE-coloured (the same colour
--             a type has in a real .rs buffer), and `field` is a property.
--------------------------------------------------------------------------------
nx.complete.source({
  name = "hoverdemo",
  debounce = 0,
  complete = function(ctx)
    local items = {
      -- 1. a field hover: framed as a struct body
      { text = "field", doc = "```rust\nfield: Vec<String>\n```" },
      -- 2. a statement hover: framed as a function body
      { text = "let", doc = "```rust\nlet total = counts.len();\n```" },
      -- 3. a body-less signature: parses on its own, no framing needed
      { text = "sig", doc = "```rust\npub fn frobnicate(x: &str) -> Option<String>\n```" },
      -- 4. an annotation dialect: no framing fits, so nothing is guessed
      { text = "dia", doc = "```rust\n(method) Registry::get(name: &str) -> bool\n```" },
      -- 5. a python signature with no body: framed by giving it a `:` and a `pass`
      { text = "pyd", doc = "```python\ndef frobnicate(name: str, count: int) -> bool\n```" },
      -- 6. a python class header, likewise
      { text = "pyc", doc = "```python\nclass Registry(Mapping)\n```" },
      -- 7. pyright's shape: a display label in front of a bare signature
      { text = "pym", doc = "```python\n(method) join(self, sep: str) -> str\n```" },
      -- 8. ty's shape: one block holding every overload as its own line
      {
        text = "pyo",
        doc = "```python\ndef join(self, x: str) -> str\ndef join(self, x: bytes) -> bytes\n```",
      },
    }
    for _, item in ipairs(items) do
      if item.text:find(ctx.prefix, 1, true) == 1 then
        ctx.push(item)
      end
    end
  end,
})
nx.complete.setup({ sources = { { "hoverdemo" } } })

--------------------------------------------------------------------------------
-- 2. A DIALECT is left alone rather than guessed at.
--
-- `(method) Registry::get(name: &str) -> bool` is what tsserver-style servers
-- put in a fence: display text, not source. No framing makes it parse, so the
-- highlighter paints only what it can vouch for — the tokens the *lexer* got
-- right — and refuses to name any construct.
--
-- Type-this:  <Esc> d d i d i a <C-n>
-- See-that:   punctuation and operators are coloured; `Registry`, `get` and
--             `name` are plain. Nothing is confidently wrong.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 2b. INDENTATION-SENSITIVE languages.
--
-- A python hover is usually a header with no body (`def f(a: int) -> bool`), which
-- is not a statement — the shipped framing gives it a `:` and a `pass`. And when a
-- template's `%s` follows only whitespace, that whitespace is the block level the
-- fragment sits at, so EVERY line of the fragment is indented to match; without
-- that a multi-line python fragment would be framed as a header, one indented
-- line, and then a dedent, which is a syntax error rather than a block.
--
-- Type-this:  <Esc> d d i p y d <C-n>       (then again with `pyc`)
-- See-that:   `def` is a keyword, `frobnicate` a function, `name` / `count`
--             parameters, and `str` / `int` / `bool` types — a real parse, from a
--             fragment that is not a python program.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 2c. A DISPLAY LABEL, and a block that is a LIST.
--
-- Two shapes the ladder is run for you on, because as written no framing can take
-- either. `pyright` puts its own label in front of the code — `(method) `,
-- `(class) `, `(type alias) ` — which is what stops the signature behind it from
-- framing; it is peeled off, the ladder runs on the rest, and the label is painted
-- like a comment. And `ty` answers a hover on an overloaded function with EVERY
-- signature, one per line: together they are a fragment of nothing, so each line is
-- resolved in its own right.
--
-- Both are all-or-nothing. The dialect in section 2 is peeled too — and what is
-- left still fits no framing, so the peel leaves no trace and the block falls to
-- the repaint whole, exactly as before.
--
-- Type-this:  <Esc> d d i p y m <C-n>       (then again with `pyo`)
-- See-that:   `pym` — `(method)` is dimmed like a comment while `join` is a
--             function and `sep` / `str` a parameter and a type.
--             `pyo` — BOTH overload rows are fully coloured, not just the first.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 3. Teach it a framing of your own.
--
-- `fragment_context` replaces a language's list of framings. Each template holds
-- one `%s` where the snippet goes, and the first one that parses cleanly wins.
-- Here rust also learns the `impl` body, so a bare `fn` signature hover with a
-- body picks up its associated-item structure.
--
-- Type-this:  (nothing — this runs at startup)
-- See-that:   section 1 still works; the extra framing is simply tried in turn.
--------------------------------------------------------------------------------
nx.treesitter.fragment_context("rust", {
  "struct __nx {\n%s\n}", -- fields
  "fn __nx() {\n%s\n}", -- statements and expressions
  "impl __nx {\n%s\n}", -- associated items
  "trait __nx {\n%s\n}", -- bare signatures
})

-- An INDENTING framing: the `%s` follows four spaces, so the whole fragment is
-- placed inside the class body at that level, not just its first line.
nx.treesitter.fragment_context("python", {
  "%s:\n    pass\n", -- a header with no body: def / class / if / for / with
  "class __nx:\n%s", -- a member block that already carries its indentation
  "class __nx:\n    %s", -- a flush member block: indented line by line
  "def __nx():\n    %s", -- a flush statement block, likewise
  "def %s:\n    pass\n", -- a BARE signature: the `def` a hover dropped
})

--------------------------------------------------------------------------------
-- 4. Turn the ladder OFF to see the difference.
--
-- With no framings, a fragment can only get the conservative repaint — the same
-- treatment the dialect in section 2 gets.
--
-- Type-this:  <Esc> :FragmentLadderOff <CR> , then  d d i f i e <C-n>
-- See-that:   `Vec` and `String` are now PLAIN. They are no longer mis-coloured
--             either: the highlighter would rather say nothing than guess.
--             `:FragmentLadderOn` puts the framings back.
--------------------------------------------------------------------------------
nx.command("FragmentLadderOff", function()
  nx.treesitter.fragment_context("rust", {})
  nx.notify("rust fragment framings: off")
end, {})

nx.command("FragmentLadderOn", function()
  nx.treesitter.fragment_context("rust", { "struct __nx {\n%s\n}", "fn __nx() {\n%s\n}" })
  nx.notify("rust fragment framings: on")
end, {})
