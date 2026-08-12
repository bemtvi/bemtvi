# Plugin Development

A plugin is **pure Lua over the `btv.*` API** — a `lua/<name>/init.lua` module
that exposes `setup(opts)` and wires keymaps, commands, autocmds, and UI through
`btv.*`. The server owns the UI surfaces; the plugin supplies data and behavior.

Install one by declaring it with the built-in manager and running `:PluginSync`:

```lua
btv.plugins({
  { "bemtvi/bemtvi-keys-helper",
    config = function() require("bemtvi-keys-helper").setup({}) end },
})
```

## Where to go next

- **[Writing plugins](authoring.md)** — the full authoring guide: anatomy,
  loading, the `btv.*` surfaces you'll use, a worked example, and testing.
- **[btv.* API Reference](../api/index.md)** — every public function in the
  `btv.*` namespace, generated from the prelude source.
- **[bemtvi-plugin-skills](https://github.com/bemtvi/bemtvi-plugin-skills)** — agent
  skills that teach a coding agent the `btv.*` model, one skill per surface. The
  model is not in any agent's training data, so without them an agent writes
  neovim plugins that do not run here.
