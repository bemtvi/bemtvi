-- gamma's real command, defined only once the plugin loads. The lazy `:GammaHello`
-- stub re-dispatches to this after loading.
btv.command("GammaHello", function()
  btv.notify("gamma says hello (it is now loaded)", 2)
end, { desc = "Greet from the lazily-loaded gamma plugin." })
