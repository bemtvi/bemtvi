# Smooth scrolling

Viewport scrolls **slide** instead of teleporting — neoscroll.nvim's behavior,
built into the core. It is on by default and needs no plugin.

When the viewport moves more than a step — `<C-d>`/`<C-u>`, `<C-f>`/`<C-b>`, the
mouse wheel, or an off-screen jump like `G`/`gg`/a search — the editor emits a
`scroll` descriptor (the from/to lines and a duration) and the client
interpolates the slide locally over its own wall clock with an ease-out curve.
Because the animation runs **client-side**, it stays smooth even over a slow
[remote daemon](../edit-host-split.md) link, and any new keystroke interrupts the
in-flight slide and snaps straight to the destination.

Single-line motions at the window edge, typing in insert/command mode, and edits
that reflow the buffer are kept crisp — only genuine viewport jumps animate.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `'scrollanim'` | `true` | Animate viewport scrolls. `:set noscrollanim` snaps every scroll. |
| `'scrollanimduration'` | `160` | The longest a slide may last, in milliseconds. The per-scroll duration scales with the travel distance and is clamped to this ceiling; `0` disables animation entirely. |

```lua
nx.o.scrollanim = true
nx.o.scrollanimduration = 160   -- ms ceiling; raise for a slower, more visible slide
```

`'scrollanim'` is also a **window-local** option (`nx.wo.scrollanim`): `nil`
inherits the global value, `false` forces this window's scrolls to snap, and
`true` forces the slide even when the global is off. A synced side-by-side diff
sets it `false` on its panes so a mirrored scroll doesn't desync.

See [`examples/smooth-scroll/`](https://github.com/davidrios/nxvim/tree/main/examples/smooth-scroll)
for a runnable demo.
