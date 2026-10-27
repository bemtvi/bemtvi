# Splitting bemtvi-gui: a toolkit-free GUI library, the editor, and the embed

**Goal.** The GUI client becomes a library that knows no toolkit, `bemtvi-gpu`, the
way `bemtvi-gui` was before it moved onto mitsuami (`ebfdf0ef`, winit + wgpu). Two thin
crates sit on it:

- **`bemtvi-gui`**, the full editor: a binary whose window is mitsuami's.
- **`bemtvi-mitsuami`**, the editor as a mitsuami view an app mounts in its own layout
  (what `bemtvi_gui::embed` is today).

Why:

- **Dependencies.** An app that embeds the editor resolves its dependencies against
  ours. Only the adapters touch mitsuami, so each can ask for it the way it needs:
  `bemtvi-mitsuami` a range an app can share, `bemtvi-gui` the exact version it ships.
  (The rest of the tree has the same problem; phase 5.)
- **Other hosts.** Nothing in the library ties the editor to mitsuami. Another toolkit,
  or winit again, is one more adapter.
- **Tests.** The client logic runs headless with real keys and real redraws. Today it
  can't: mitsuami's headless backend gives the surface no window, so the editor never
  attaches a grid and never gets a redraw. The review of the embedding work had to
  leave the disabled-`:workspace` fix untested for that reason.

Decided with the user (2026-10-26): the full editor stays on mitsuami; the library is
toolkit-free; the embeddable editor is its own crate. See *Decided* below for the rest.

## What the code gives today

Survey of `crates/bemtvi-gui/src` (2026-10-26, at `f4c66305`):

| File | Lines | mitsuami in it |
|---|---|---|
| `render.rs` | 6354 | `SurfaceHandle` / `SurfaceSize` only: `Renderer::new` hands the handle to `wgpu::Instance::create_surface` and reads the size and scale |
| `session.rs` | 653 | none (`Session`, `Launch`, `Config`, `Shada`, the spawners) |
| `images.rs`, `remote.rs`, `fontchain.rs` | 1239 | none |
| `input.rs`, `mouse.rs` | 223 | the key, modifier and mouse types (`SurfaceInput`, `KeyCode`, `Modifiers`, `MouseButton`, `ScrollDelta`) |
| `lib.rs` | 2157 | the `App`: see below |
| `embed.rs` | 648 | `View`, `Ui`, `Owner`, `on_cleanup`, `NodeRef`: the mitsuami view itself |
| `main.rs` | 167 | the CLI; `run` builds the `mitsuami::App` |

`lib.rs` splits into toolkit-free parts and the `App`'s ties to mitsuami:

- **Toolkit-free already:** `spawn_io` (the client IO thread: RPC, redraw → `View`,
  session swaps, image fetch and decode), `GuiConfig`, `GlyphOverflow`, `caret_blink`,
  `dialog_action` and its helpers, `report_session_error`.
- **The `App`'s use of mitsuami:**
  - *Tasks on mitsuami's executor* (`spawn_local`): the event pump from the IO thread,
    the paint loop (a `Notify`), the caret blink loop, the `timeoutlen` flush timer
    (`arm_flush_deadline`), the drag autoscroll timer (`arm_autoscroll`).
  - *Services:* `open_file` / `save_file` (the native dialogs), `clipboard_text` (paste).
  - *Surface state it sets:* the pointer shape (`Cursor` / `SystemCursor`), the IME
    caret rect (`text_cursor`), input methods on or off, the backdrop (`transparent`),
    focus.
  - *Input:* `input(&SurfaceInput)`, with mitsuami's key, text and pointer events.

So the work is the `App` (into a state machine that asks its host for those things) and
a renderer that takes a window handle instead of a mitsuami type. The 6354 lines of
renderer, and the sessions, images and fonts, move as they are.

## Design

### The three crates

```
bemtvi-gpu   toolkit-free: renderer, fonts, images, sessions, IO thread, Client
     ▲      ▲
     │      └── bemtvi-mitsuami   lib: the mitsuami view (Editor, EditorHandle,
     │                               SessionOptions re-exported), mitsuami as a range
     └───────── bemtvi-gui     bin: the window, process signals, the CLI, packaging;
                                    mounts bemtvi-mitsuami's view, mitsuami exact
```

`bemtvi-gui` keeps its name, binary, icons and packaging (`build.yml`, the AppImage, the
macOS bundle), so nothing outside the crates moves. The full editor's window content is
`bemtvi-mitsuami`'s view, as today it is `embed::Editor`.

### `bemtvi-gpu`: the `Client`

The `App` without mitsuami: one state machine per editor. The host feeds it events and
carries out what it asks for. Sketch:

```rust
pub struct Client { /* the App's state, minus mitsuami's */ }

impl Client {
    /// The editor on `session`. `wake` is called (from any thread) whenever the IO
    /// thread has queued something: the host then calls `pump` on its UI thread.
    pub fn new(session: Session, options: ClientOptions, wake: impl Fn() + Send + Sync + 'static) -> Client;

    /// Render to this window from now on; without one the editor runs headless.
    pub fn attach_surface(
        &mut self,
        window: impl HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
        size: SurfaceSize,
    ) -> Result<()>;
    pub fn detach_surface(&mut self);
    pub fn resize(&mut self, size: SurfaceSize);

    pub fn input(&mut self, event: InputEvent);
    pub fn pump(&mut self);                         // what the IO thread queued
    pub fn tick(&mut self, now: Instant);           // timers that came due
    pub fn next_deadline(&self) -> Option<Instant>; // when to call `tick` next
    pub fn paint(&mut self) -> bool;

    /// What the editor asks of its host since the last call.
    pub fn take_effects(&mut self) -> Vec<Effect>;
    /// The answer to a request an `Effect` made (a dialog's file, the clipboard).
    pub fn answer(&mut self, request: RequestId, answer: Answer);

    /// RPC into the editor (exec_lua, command, quit_now, request_quit), `Send`.
    pub fn handle(&self) -> ClientHandle;
    /// Stop the IO thread and join every server thread; whether one panicked.
    pub fn close(self) -> bool;
}

pub enum Effect {
    Redraw,
    Cursor(CursorShape),          // arrow, text, resize ↔ / ↕
    ImeArea(Option<Rect>),
    InputMethod(bool),            // 'imdisable'
    Transparent(bool),            // 'guiopacity' below 100
    OpenDialog { request: RequestId, title: String, start_dir: Option<PathBuf> },
    SaveDialog { request: RequestId, title: String },
    ReadClipboard { request: RequestId },
    Exit,                         // bemtvi_exit, or the connection closed
    Host { name: String, payload: Value }, // btv.host.send
}
```

- **Timers become deadlines.** The blink, the `timeoutlen` flush and the drag
  autoscroll each keep an `Instant`; `next_deadline` is the earliest, and `tick` does
  what came due. The host sleeps until then. No task on any executor.
- **The waker.** `spawn_io` keeps its tokio channel to the UI thread and calls `wake`
  after each send. Under mitsuami the adapter's `wake` notifies a `tokio::sync::Notify`
  that a `spawn_local` task awaits, then calls `pump`.
- **Requests answered later.** A dialog or a clipboard read is an effect with a
  `RequestId`; the host calls `answer` when the platform replies. `run_picked` and
  `paste_clipboard` become the `answer` paths. A host without dialogs turns them off in
  `ClientOptions` (today's `native_dialogs(false)`).
- **The renderer takes a window handle.** `Renderer::new` already passes the handle
  straight to `create_surface`; it takes `raw-window-handle` 0.6's traits instead of
  `SurfaceHandle`, which wgpu 30 and mitsuami already share. wgpu stays private to the
  library: no wgpu type crosses its API. The process-wide `Gpu` stays as it is, and so
  does the Windows composition-visual swapchain (it's per surface, on whatever HWND the
  handle names).
- **What moves in unchanged:** `session.rs` (`Session`, `Launch`, `Config`, `Shada`,
  the spawners), `remote.rs`, `images.rs`, `fontchain.rs`, `spawn_io`, `GuiConfig`,
  `dialog_action`.
- **`SessionOptions` and the single-file preset move to the library.** Nothing in them
  is mitsuami's, and another host should get the same preset. `bemtvi-mitsuami`
  re-exports them, so its API stays as it is.

### Input without a toolkit

`InputEvent` carries what mitsuami's `SurfaceInput` carries today, in the library's own
types: a key (its code, the modifiers held, and the text it typed when it typed text, so
layouts, dead keys, AltGr and input methods keep working), committed text, the
preedit, pointer moves, buttons and wheel deltas in physical pixels, and focus.
`encode_input` and the mouse helpers work on these. Each adapter translates its
toolkit's events; the translation is small and tested on its own.

### `bemtvi-mitsuami`

`embed.rs` becomes the crate: `Editor` builds a `GpuSurface`, makes a `Client` on it,
and runs three `spawn_local` tasks (the pump on the waker's `Notify`, the deadline
timer, the paint loop) plus the effects: the pointer shape, the IME rect, input
methods, the backdrop, the dialogs and the clipboard through mitsuami's services,
`Exit` to `on_exit`, `Host` to `on_event`. `EditorHandle` wraps `ClientHandle` and keeps
its UI-thread lifecycle (mount, unmount, remount, `close`).

## Dependencies (phase 5)

The split puts mitsuami's requirement in the two adapters only. The rest of the tree
still pins exactly through `[workspace.dependencies]` (`tokio = "=1.53.2"`,
`wgpu = "=30.0.1"`, `mlua = "=0.12.2"`, …), and an app that embeds the editor resolves
against all of it. Cargo allows one semver-compatible version of a crate per build, so
an app on tokio 1.54 can't build against `=1.53.2`.

What exact pins give the binaries, the committed `Cargo.lock` already gives: every
version, transitive ones included, fixed until someone moves it. So:

- **Libraries an app links** (`bemtvi-gpu`, `bemtvi-mitsuami` and everything under
  them: `bemtvi-server`, `-core`, `-lua`, `-rpc`, `-view`, `-ts`, `-git`, …) use ranges,
  the version we test as the floor: `tokio = "1.53.2"`. mitsuami in `bemtvi-mitsuami` is a
  range too, once a release has what it needs (it's a git revision until then).
- **The binaries** (`bemtvi`, `bemtvi-gui`) get exact versions from `Cargo.lock`, built
  with `--locked` in CI and the release workflow (`build.yml` doesn't pass it today).
  `bemtvi-gui` can also keep `=` on mitsuami in its own manifest; in one workspace it
  and `bemtvi-mitsuami`'s range resolve to the same locked version.
- **Upgrades** still follow the two-week floor, through `Cargo.lock`, and through the
  floor in a manifest when an API we use needs a newer version.
- **This changes a standing rule,** with the user's agreement: "pinned exact
  dependencies where possible" becomes exact for what we ship as binaries (by the lock,
  built `--locked`), ranges for what others link.
- A CI job building with `-Z minimal-versions` would catch a floor set too low. Optional.

## Phases

Each phase is committed and paused for review. `cargo test --workspace` is green at the
end of each, and `bemtvi-gui` behaves as before throughout.

1. **The library crate, renderer on a window handle.** Create `bemtvi-gpu`; move
   `render.rs` (`Renderer::new` on `raw-window-handle`), `session.rs`, `remote.rs`,
   `images.rs`, `fontchain.rs`, `GuiConfig` and friends, `caret_blink`,
   `dialog_action`, `spawn_io`. `bemtvi-gui` re-exports what it exported, so its tests
   and `main.rs` don't change. The toolkit-free tests (`blocks`, `caret`, `color`,
   `cursor`, `fonts`, `grid`, `wide`, `syntax`, …) move with the code.
2. **Toolkit-free input.** `InputEvent` and its key, modifier and pointer types;
   `encode_input` and the mouse helpers on them; mitsuami → `InputEvent` in
   `bemtvi-gui`. `keys.rs` and `mouse.rs` are rewritten on the library's types, plus a
   test of the mitsuami translation (the wheel's sign convention among them).
3. **The `Client`.** The `App`'s logic moves into the library behind the API above:
   effects, deadlines, the waker, requests answered later. The `App` that's left in
   `bemtvi-gui` is the adapter. New headless tests in `bemtvi-gpu/tests`, a real
   server behind a `Client` with no surface, driving keys and redraws: the `:workspace`
   interception (switching off, and a disabled `:workspace`), `<CR>` on `:e` asking for
   the open dialog and `answer` running `:e <file>` escaped, a paste asking for the
   clipboard, the `timeoutlen` flush firing from `tick`, the autoscroll deadline, `Exit`
   once.
4. **`bemtvi-mitsuami`.** `embed.rs`, the mitsuami adapter, `tests/embed.rs` and the
   `embed` / `single_file` cargo examples move into the new crate. `bemtvi-gui` becomes
   the binary: `main.rs`, `run`, the window, signals, packaging. Docs:
   `docs/features/embedding.md` (the dependency is `bemtvi-mitsuami`),
   `docs/architecture.md` (the crate layout), the book page.
5. **Dependency ranges.** As above: the library crates' manifests
   on ranges, `--locked` in CI and release, the Cargo.toml comments and the upgrade
   policy updated, CLAUDE.md's dependency convention rewritten to match.

## Decided (2026-10-26)

- **Names:** `bemtvi-gpu` (the toolkit-free library) and `bemtvi-mitsuami` (the mitsuami
  adapter an app embeds). `bemtvi-gui` keeps its name as the binary.
- **Publishing:** the libraries go to crates.io later. For now apps depend on them by
  git, and mitsuami stays a git revision.
- **Phase 5 is on:** ranges for the libraries, `--locked` for the binaries.

## Risks

- **`raw-window-handle` must be the same version** in mitsuami and wgpu: 0.6 in both
  today. A mitsuami that moves first would break phase 1's handoff until wgpu follows.
- **Text input differs by toolkit.** mitsuami reports the text a key typed alongside
  the key; winit does too, others may not. `InputEvent` follows that model, so an
  adapter for a toolkit that doesn't would have to synthesize the text.
- **Phase 3 is the large one.** The `App`'s `Rc<RefCell<…>>` tasks become plain
  methods, and the order things happen in (a redraw's view, then the IME rect, then the
  pointer shape) must survive the move. The headless tests are there to hold it, and
  the GUI should be checked by hand on each platform before the phase is called done.

## As built (2026-10-27)

All five phases landed, one commit each. Where the build differs from the design above:

- **Pointer positions are points,** not physical pixels: mitsuami reports points and
  the client already scaled them by the surface's scale, so `InputEvent` carries points
  and the `Client` keeps the scaling.
- **`Client::new` returns `(Client, IoThread)`** rather than the client owning a
  `close`: an unmounted editor's IO thread is stopped at once and joined later (by
  `EditorHandle::close`), so the two have different lifetimes.
- **`Client::attach_headless(cols, rows)`** is new: a client with no renderer still
  attaches a grid, so it gets frames. That's what the headless tests drive
  (`crates/bemtvi-gpu/tests/client.rs`).
- **`ClientOptions::option_chords`** carries macOS's Option-makes-chords policy, which
  the `App` read from `cfg!` directly.
- **A message nobody listens for** (`btv.host.send` with no `on_event`) is reported by
  the mitsuami adapter, which knows whether the app listens; the client always emits
  `Effect::Host`.
- **`keys.rs` and `mouse.rs` stay over mitsuami's types** (in `bemtvi-mitsuami`), as
  end-to-end tests of the translation and the encoder; `bemtvi-gpu/tests/input.rs`
  covers the encoder on its own types.
- **Exit once** is guarded in the client, but no test can see a second `Exit` from the
  IO thread on the paths tried (only one arrives), so the guard is untested.
- **Phase 5:** every `[workspace.dependencies]` requirement is a caret range at the
  version it was pinned to, except `ropey` (a pre-release, still `=`). `Cargo.lock` is
  unchanged by it. CI's binary builds and `bemtvi-edithost/build.sh` build `--locked`.
  `bemtvi-edithost` (workspace-excluded, its own lock) keeps its exact pins.

