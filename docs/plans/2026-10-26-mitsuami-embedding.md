# Embedding bemtvi in a mitsuami application

**Goal.** A mitsuami app can put a bemtvi editor anywhere in its own layout — a pane
beside a sidebar, a tab, a dialog — as one component, the way it puts a `TextArea`
there. The editor inside is the whole editor (core, Lua, config, plugins, LSP, the
edit-host split), not a reduced one. The host talks to it through a handle and hears
back through events, and the editor's Lua can address the host.

`bemtvi-gui` already is this component, minus the packaging: its window's whole content
is one `GpuSurface` (`crates/bemtvi-gui/src/lib.rs`, `App::window`), and the state
behind it — the server thread, the IO thread, the renderer, the paint/blink/event
`spawn_local` tasks — never touches the window except through that surface. What stops
a host from using it is `bemtvi_gui::run`, which assumes it owns the **process**. The
work is to take those assumptions out and give the host a way in and out.

## What assumes it owns the process

| Today (`run`) | Embedded |
|---|---|
| Builds and runs `mitsuami::App`, owns the main thread until the window closes | The component mounts inside the host's `App` and window. Threads start on mount and are torn down on unmount (IO thread notified, server sees EOF, both joined). |
| `bemtvi_view::signals::install` (SIGINT/SIGHUP → `:qa`), `exit_as_signal_if_killed` | Not the component's — process signals are the host's. Stays in `bemtvi-gui`'s `run`. |
| `bemtvi_exit` closes the window (`open` signal false) | `EditorEvent::Exit` to the host, which decides (close the tab, hide the pane, quit). |
| Close button → `on_close_request` → `:qa` | The host's window. The handle's `request_quit()` runs the same `:qa`, so a host keeps the `E37` unsaved-buffer refusal if it routes its close through it. |
| `std::mem::forget(renderer)` (wgpu's EGL `eglTerminate` segfault on Wayland + Mesa when the `Instance` drops) | Safe only because `run` returns as the process exits; a component that mounts and unmounts would leak a renderer each time. The crash is in the **`Instance`'s** drop, so: one process-wide `Instance`/`Adapter`/`Device` in a `static` (statics never drop — the same effect as the `forget`, once), and each editor's surface, pipelines and glyph atlas drop normally on unmount. Two editors then share one device too. |
| `GuiConfig::from_env` (`BEMTVI_GUI_FONT*`) | The host passes font/size/emoji/overflow explicitly; the component reads no environment. `bemtvi-gui`'s `run` keeps reading it and passes the result. |
| `default_runtime()` config (`$BEMTVI_CONFIG` → XDG) and `default_shada()` | A host choice (see *Config and shada*). |
| `:connect` / `:workspace` swap the window's whole session | Opt-in. Off, both fail loud (`echoerr` naming the command and that the host disabled it), never a silent no-op. |
| Native open/save dialogs on `:e`/`:w` (`dialog_action`) | Kept (they open on the host's window through mitsuami's services); opt-out prop for a host that wants the server's own prompts. |

Not a problem, contrary to first guess: two editors (or an embedded one plus a standalone
bemtvi) sharing the global shada. `crates/bemtvi-server/src/shada.rs` already gives each
instance its own `<pid>.<nanos>.<seq>.redb` and recency-merges the others, because redb
takes an exclusive lock.

## What mitsuami gives, and where it falls short

From `~/work/mitsuami/docs/ARCHITECTURE.md` §13.26 (`GpuSurface`):

- **Keys go to the window's shortcuts first,** then to the focused surface — Tab
  included. So a host menu accelerator beats the editor for the same chord, which is the
  right default. **Gap:** on WinUI, Control/Alt chords are reported to the surface *and*
  left unhandled so XAML's accelerators still run, so a chord the host also binds reaches
  **both**. Close it in mitsuami (report a chord to the surface only when no accelerator
  in the window matches it, as GTK's backend already does with `ShortcutTrigger::trigger`).
  Kirigami's non-grab path needs checking for the same thing.
- **Leaving the surface by keyboard:** Control+Tab leaves it on AppKit, GTK and Kirigami.
  **Gap:** not on WinUI. Close it in mitsuami. Consequence for bemtvi: a `<C-Tab>` mapping
  never fires in any mitsuami surface (already true of `bemtvi-gui`); the embed docs say so.
- **The handle outlives the widget:** the native surface is freed with the last
  `SurfaceHandle`, so the renderer dropping its wgpu surface on unmount (before the
  handle) is the required order, and mitsuami already guarantees the rest.
- **Layering:** on GTK, Kirigami and WinUI the surface sits *above* the window's own
  content, so nothing the toolkit draws in-window can overlap the editor. A host's
  popovers/menus must be the platform's own popup windows (GTK popovers and Kirigami's
  windowed `Select` are). A host constraint to document, not something bemtvi can fix.
- **Headless:** mitsuami's headless backend gives a `SurfaceHandle` with no window
  handles. The component must run without a renderer there (server, events, handle all
  live; no paint), which is what makes it testable with `mitsuami-test`.
- **Several surfaces per window:** the docs speak of "the window's surfaces" (WinUI hides
  them all under a `ContentDialog`), so it's meant to work; bemtvi has never run two.
  Covered by a test in phase 2.

## The API

The component lives in `bemtvi-gui`'s library (it already holds the renderer, images and
session code); `bemtvi-gui`'s binary becomes its first consumer.

```rust
use bemtvi_gui::embed::{Editor, EditorHandle, EditorEvent, Config, Shada};

let editor = EditorHandle::new();
view! {
    <Row>
        <Sidebar>...</Sidebar>
        <Editor
            handle=editor.clone()
            file="notes.md"
            config=Config::Dir(app_config_dir)   // Config::User | Config::None
            shada=Shada::Namespace("myapp".into()) // Shada::Global | Shada::Off
            font=font                             // family list + size, emoji/overflow
            on_exit=move || tabs.close_current()  // required
            @event=move |e: EditorEvent| ...      // Host { name, payload }, …
        />
    </Row>
}
```

- **`on_exit` is required** (a builder argument, not a setter), so "the editor quit and
  nothing happened" can't be the default.
- **`EditorHandle`** wraps the session's `Rpc`: `command(&str)`, `exec_lua(code, args) ->
  impl Future<Output = Result<Value>>`, `open(path)`, `request_quit()`, `focus()`. All but
  `focus` are calls the wire already has. To verify in phase 4: an `Rpc::request` future
  (a tokio oneshot answered by the IO thread's reader) completes when polled by mitsuami's
  executor through `spawn_local`; if not, the handle relays through the IO thread as
  `bemtvi_image_read` does.
- **Remote:** `.connect(ConnectTarget)` builds the session through the existing
  `spawn_session`; the edit-host split is per session, so an embedded editor over a daemon
  needs nothing new. Tier-1, so it gets the same verification as local.

### Editor → host: `btv.host.send`

The one real gap on the server side: Lua has no way to send the client a message of its
own. Today the only server → client notifications are `bemtvi_exit`, `bemtvi_image_read`,
`bemtvi_input_flush`, `bemtvi_panel_is_open` and `bemtvi_resolve_capture`.

- The embedding client declares a `host` capability in `nvim_ui_attach` (beside
  `truecolor`, `osc52`, `key_labels` in `dispatch.rs`) carrying the host's app id;
  `btv.ui.caps().host` mirrors it (`nil` when not embedded), so a config can branch.
- `btv.host.send(name, payload)` queues a `bemtvi_host` notification `[name, payload]`;
  the component delivers it as `EditorEvent::Host { name, payload }`. With no host
  attached it **raises** (`btv.host.send: no embedding host attached`), per the no-silent-
  stubs rule — a config that wants to work both ways checks `btv.ui.caps().host` first.
- Host → editor needs nothing new: `exec_lua`, or `command("doautocmd User …")` to fire a
  `User` event a config listens for.
- Change/write notifications (`EditorEvent::Changed`, `Wrote(path)`) are **not** new
  wire: an opt-in prelude helper (`prelude/host.lua`) subscribes to `TextChanged` /
  `TextChangedI` (debounced with `btv.utils.debounce`) and `BufWritePost` and forwards
  them through `btv.host.send`. A host that wants the text asks for it (`exec_lua`
  returning `btv.buf.lines`); pushing the whole buffer on every change would break the
  never-freeze rule on a large file.

## Config and shada

- `Config::User` — what `bemtvi-gui` does (`default_runtime()`): the user's own config.
  Right for a developer tool whose users already run bemtvi.
- `Config::Dir(path)` — the app's own config dir as `config_dir`, and on the runtimepath,
  so an app can ship its keymaps and plugins.
- `Config::None` — no `init.lua`; the prelude and bundled plugins only.
- `Shada::Global` (`default_shada()`), `Shada::Namespace(ns)` (the existing
  `workspace_shada(Some(ns))` store, so an app's marks/history stay apart from the user's
  own bemtvi), `Shada::Off` (`None` — nothing persisted).

These map straight onto `ServerInit` fields `server_init` already fills; the change is
making them parameters instead of fixed choices.

## Phases

1. **Extract the component, no behaviour change.** `embed::Editor` builds the
   `GpuSurface` and owns the server/IO/renderer state; `run` becomes
   `Window::new("bemtvi").content(|| Editor …)` plus what stays the app's (signals,
   `from_env`, `open_dir`, `:connect`/`:workspace`, close → `:qa`, exit → close window).
   The existing `bemtvi-gui` tests and a by-hand run on GTK guard it.
2. **Lifecycle.** Mount/unmount teardown on the component's cleanup; the process-wide
   static wgpu instance/device replacing the `forget`; a renderer-less headless path;
   `on_exit` required. Tests (`crates/bemtvi-gui/tests/embed.rs`, `harness = false` with
   `mitsuami-test`, headless): mount → `exec_lua` → lines; `:qa` → `on_exit` fires once;
   unmount joins the server thread; 20 mount/unmount cycles leave no threads behind; two
   editors in one window edit independently. Native: by hand on GTK under Sway (no EGL
   segfault on quit after several remounts).
3. **Config, shada, opt-ins.** `Config`/`Shada`, the native-dialogs prop, and
   session-switching off by default with `:connect`/`:workspace` failing loud. Tests
   assert `Config::None` sources no `init.lua`, `Config::Dir` does, `Shada::Off` writes
   no store file, and the loud error text.
4. **The host channel.** `host` capability, `btv.ui.caps().host`, `btv.host.send` →
   `bemtvi_host` (server + prelude; server harness tests for the notification and for the
   loud error without a host), `EditorEvent::Host`, the `EditorHandle` verbs, and the
   `prelude/host.lua` change/write helper. Book: `btv.host` API page; the docstring rules
   apply. Verified over a stdio daemon session too.
5. **Keys and focus.** In mitsuami (its own tests and docs, per its CLAUDE.md): WinUI
   chords matching a window accelerator go to the accelerator only; Control+Tab leaves a
   WinUI surface. In bemtvi: a book page *Embedding bemtvi* (API, the layering constraint,
   the reserved chords, `<C-Tab>`), and `crates/bemtvi-gui/examples/embed.rs` — a window
   with a file list, an editor pane and a line showing `EditorEvent`s, with a menu
   accelerator colliding with an editor chord to show which wins.

Each phase is committed and paused for review before the next.

## As built

All five phases landed, with these departures from the plan above:

- **Config and shada came in phase 2**, not 3: the lifecycle tests needed hermetic
  sessions. Phase 3 kept the opt-ins and the tests for them.
- **The host is an init field, not a `ui_attach` capability.** An attach comes only
  with a renderer, so a headless or not-yet-painted editor (and `init.lua`) would have
  seen no host. `ServerInit::host` is seeded before any config runs, and the Lua side is
  `btv.host.name()` rather than `btv.ui.caps().host`.
- **Session switching is refused in the VM.** The client sets
  `btv.session._switching = false` at startup, so `:connect` and
  `btv.session.reconnect` fail with a message before anything reaches the window;
  `:workspace` isn't registered at all (its body is a no-op only the window's `<CR>`
  intercept gives meaning). The window still refuses a swap notification as a backstop.
- **A deadlock the plan didn't foresee:** the IO thread joined its servers inside
  `block_on`, where the tasks owning the connections never run again. `bemtvi-gui`
  never hit it because its server had always quit by `:qa`; an unmounted editor's had
  not. The thread now leaves and drops its runtime before joining.
- **The surface grows instead of filling 100%×100%**, so it sits beside an app's own
  controls in a row or column.
- **Control+Tab already left a WinUI surface** (the window's Tab handler); mitsuami's
  doc just didn't say so. The WinUI chord fix is mitsuami `59db232`, only type-checked
  from Linux.

## Still open

- **Version coupling.** mitsuami's types cross the component's API (`Editor` is a
  mitsuami `View`), so an exact pin forces every host onto bemtvi's exact mitsuami, and a
  git-rev pin onto the same git source. Decided: once a mitsuami release carries the
  embedding fixes, mitsuami goes to a caret requirement in `[workspace.dependencies]`
  (the exception to the exact-pin rule, since it's shared with dependents), and
  `Cargo.lock` keeps bemtvi's own builds exact. wgpu and glyphon stay exact: they don't
  cross the API.
- **Telling the editor which chords the host keeps.** The host's accelerators win, so an
  editor keymap on the same chord silently never fires. A `reserved_keys` capability
  (like `key_labels`) would let `btv.keymap.set` warn and a which-key popup show it as
  taken. Worth doing if hosts turn out to bind many Ctrl chords.
- **One IO thread per editor.** Fine for a handful; a host with many editors (a notebook)
  would want one shared client runtime. Not before someone needs it.
- **Non-Rust hosts.** `mitsuami-ffi` exposes mitsuami through a C ABI; the component could
  follow once its Rust API settles.
- **`Editor::native_dialogs` has no test**: the `<CR>` intercept reads the redraw-fed
  view, which the headless suite doesn't get.
