# Indent detection

A file's **own indentation** decides how you indent it. Open a tab-indented file
in a spaces-by-default config and `>>` inserts a tab; open a 2-space file and
`<Tab>` inserts two spaces. This is what the vim-sleuth plugin does for vim — in
bemtvi it is built into the core, on by default, with nothing to install.

Every time bemtvi reads a file it looks at the leading whitespace of that file's
own lines and sets the buffer's `'expandtab'` and `'shiftwidth'` from what it
finds:

* Indented with **tabs** → `'noexpandtab'`, and `shiftwidth=0` (bemtvi's "follow
  `'tabstop'`" sentinel), because one indent level in such a file is one tab.
* Indented with **spaces** → `'expandtab'`, and `'shiftwidth'` set to the step
  the file's own lines show (2, 4, …).
* **No usable evidence** — an empty or unindented file, a single indented line,
  or tabs and spaces exactly as common as each other → nothing changes, and the
  style your config chose stands.

The verdict is **per buffer**, not a mode: a tab-indented file and a 2-space file
open side by side each keep their own.

## Precedence

The file beats your config; anything set *after* the read beats the file.

```
built-in defaults  <  your config  <  the file's own indentation  <  :set / an autocmd / .editorconfig
```

Detection runs when the bytes become the buffer's text — before `BufReadPost` —
so an autocmd, an [`.editorconfig`](https://github.com/bemtvi/bemtvi-editorconfig)
plugin, or you typing `:setlocal noexpandtab` all have the last word. Re-reading
the file (`:e!`, an autoread reload) runs the detection again, so a file
reindented on disk is picked up.

It works identically on a local file, a file fetched over a
[daemon](../edit-host-split.md) link, and a file read from OPFS in the
[browser](../browser-editor.md): the detection hangs off the point where a read's
bytes become the buffer's text, which is core-side on every one of those legs.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `'indentdetect'` (`'idt'`) | `true` | Let an opened file's own indentation set its `'expandtab'` and `'shiftwidth'`. |

```lua
btv.o.indentdetect = false   -- or :set noindentdetect
```

With it off, every buffer simply uses the `'expandtab'` / `'shiftwidth'` your
config set, as it did before.

## How the width is decided

The step, not the raw indent width, is what is counted: a 2-space file's indent
widths are 2, 4, 6, 8 — which alone look as much like a 4-space file with skipped
levels — while every *step* between one line and the next is 2. The most common
step wins, ties going to the narrower one.

Three deliberate abstentions keep a stray line from setting the style for a whole
file:

* A **one-column step** is never adopted as a width. It is far more often a
  wrapped line or prose than a convention, and a silent `shiftwidth=1` is a much
  worse outcome than leaving the configured width alone.
* A file must show at least **two** indented lines before its width is taken —
  one indented line is as likely a misindent as a convention. (The
  tabs-versus-spaces direction is still read from a single line: that much a lone
  indented line does say.)
* **Block-comment bodies** (` * ` continuation lines) are skipped, so the space
  that aligns a star under its `/*` neither votes for a 1-column indent nor makes
  a tab-indented C file look space-indented.

The scan is bounded — it stops after enough indented lines to be sure, and never
reads more than the first 8192 lines — so opening a huge file costs
nothing measurable.

See [`examples/indent-detect/`](https://github.com/bemtvi/bemtvi/tree/main/examples/indent-detect)
for a runnable demo.
