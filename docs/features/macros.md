# Keyboard macros

Record a sequence of keys once, then replay it as often as you like. The
behaviour is vim's; the bindings are bemtvi's.

```
<F2>{reg}     start recording into register {reg}   (uppercase appends)
<F2>          stop recording
<F3>{reg}     play register {reg} back
{count}<F3>{reg}   play it {count} times
<F3><F3>      play the last register again
<F3>:         re-run the last ex command
```

While a recording is open the message line reads `recording @a`, exactly as vim
announces it, and the `macro` statusline segment shows the same.

## Why not `q` and `@`?

`q` is the close key on half of bemtvi's read-only surfaces (a `btv.view` pane, a
dock, the plugin dashboard), and vim's `q` shadows a genuinely useful key for
every user who never records a macro. So the trigger moved somewhere nothing else
wants. If your fingers disagree, one line brings vim's spelling back:

```lua
btv.keymap.set("n", "q", "<F2>")
btv.keymap.set("n", "@", "<F3>")
```

That works — rather than merely typing the F-key's name — because of how
recording is built: see below.

## A macro is a register, and a register is text

A recording is stored as ordinary **key notation** in an ordinary register:

```
<F2>a  0ciwbeta<Esc>j  <F2>        →  register a = "0ciwbeta<Esc>j"
```

Which means everything you already know about registers applies. `"ap` pastes the
macro into the buffer as editable text. `:registers` lists it. It persists across
sessions through shada, and syncs over a daemon or browser session like any other
register. And you can write one by hand, no recording needed:

```lua
btv.reg.set("a", "0ciwbeta<Esc>j")
```

The one thing to know: because the register is parsed as *keys*, playing a
register that happens to hold ordinary yanked text will read any `<...>` in that
text as a key name rather than as literal characters.

## Recording captures what you typed, mappings included

bemtvi records the keys you pressed — not what they resolved to — and replays them
through the keymap matcher, so the mappings fire again. This matters here more
than it does in vim, because so much of bemtvi is Lua keymaps: the LSP `gd`, the
completion triggers, every plugin's bindings. A recording that captured only what
reached the editor would silently drop them and the replay would do nothing.

So a macro over `<leader>f` records `<leader>f` and re-fires it on playback. It is
also why remapping `q` to `<F2>` above works at all.

Keys that were *not* typed stay out: a mapping's own RHS, `nvim_feedkeys`
typeahead, and the keys of a macro that is playing back. Recording `<F3>b` stores
those two keys, not everything `b` expands to.

## A failed keystroke ends the playback

The classic idiom is to record one line's worth of edit and then replay it far
more times than there are lines:

```
<F2>a  I- <Esc>j  <F2>       record: prefix this line, go down
99<F3>a                       …and do that until it stops
```

`j` on the last line **fails**, and a failure ends the whole run — every remaining
repeat and every suspended macro. Without that, the extra repeats would keep
prefixing the last line. Anything vim would beep at counts: a motion already at
the buffer edge, an unmatched `f{char}`, a search that finds nothing, any `E###`
error.

## Macros can call macros

`<F3>b` inside macro `a` suspends `a`, runs `b` to completion, and resumes. A
macro that calls *itself* is legal — vim relies on the first failing command to
end the recursion, and so does bemtvi — with a depth cap and a key budget behind
it that report loudly rather than hanging.

## From Lua

```lua
btv.macro.recording()   -- register being recorded into, or nil
btv.macro.executing()   -- register playing back right now, or nil (innermost)
btv.macro.play("a", 2)  -- play register a twice
```

`vim.fn.reg_recording()` / `vim.fn.reg_executing()` are the vim-shaped aliases
(they return `""` rather than `nil`). `btv.macro.executing()` is the cheap way for
a plugin to skip work no human is watching — an expensive redraw during a long
`100<F3>a`, say.

The `macro` statusline segment is built in:

```lua
btv.statusline.setup({ left = { "mode", "macro", "filename" }, right = { "location" } })
```
