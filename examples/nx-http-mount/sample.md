# Live markdown preview

Press `\p` to open this file in your browser, then **edit this buffer** — the page
follows along within half a second.

This document is served by the editor itself, over `nx.http.mount`.

## How it works

The plugin never binds a port. It *mounts* a subroute:

```lua
nx.http.mount({
  name = "preview",
  on_request = function(req, respond)
    respond({ body = "<h1>hi</h1>" })
  end,
}):next(function(m)
  nx.ui.open(m:url())     -- http://127.0.0.1:53124/plugin/preview/
end)
```

The editor owns the one listener; every plugin hangs off it at `/plugin/<name>/*`.
That is what lets the same plugin run in the browser build, where a tab cannot bind a
port and a Service Worker answers the same routes instead.

## Things to try

- Edit this sentence and watch the page update.
- Press `\u` to print the mount's URL and the shared origin.
- Press `\c` to close the mount, then reload the page — the editor 404s it.
- Open `<url>info?pretty=1` for the JSON endpoint.
- Add a `## heading`, a list, or a code block — the *renderer is in the plugin*, so
  this is the demo page's JS doing the work, not the editor.

> The editor's job is to serve bytes. What renders them is the plugin's business —
> use whatever frontend framework you like, and let it own live-reload.

## Formatting sampler

| what | where |
| ---- | ----- |
| the listener | the editor |
| the renderer | the plugin |

1. `nx.o.httpport = 8080` pins a stable, bookmarkable port.
2. `nx.o.httphost` defaults to `127.0.0.1` — loopback only.
3. Setting either while mounted *rebinds*, and every mount's URL moves with it.

Some ~~struck~~ text, a [link](https://github.com/davidrios/nxvim), and `inline code`.

---

*Nothing starts until a plugin asks: with no mount, the editor opens no port at all.*
