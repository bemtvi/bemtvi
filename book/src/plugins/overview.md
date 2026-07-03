# Plugin Development

A plugin is **pure Lua over the `nx.*` API** — a `lua/<name>/init.lua` module
that exposes `setup(opts)` and wires keymaps, commands, autocmds, and UI through
`nx.*`. The server owns the UI surfaces; the plugin supplies data and behavior.

Install one by declaring it with the built-in manager and running `:PluginSync`:

```lua
nx.plugins({
  { "nxvim/nxvim-keys-helper",
    config = function() require("nxvim-keys-helper").setup({}) end },
})
```

## Where to go next

- **[Writing plugins](authoring.md)** — the full authoring guide: anatomy,
  loading, the `nx.*` surfaces you'll use, a worked example, and testing.
- **[nx.* API Reference](../api/index.md)** — every public function in the
  `nx.*` namespace, generated from the prelude source.
