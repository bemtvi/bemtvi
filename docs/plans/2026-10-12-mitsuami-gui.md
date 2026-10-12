# bemtvi-gui on mitsuami (replacing winit)

**Goal.** `bemtvi-gui` drops winit and runs on [mitsuami](https://mitsuami.roboport.com.br)
(`~/work/mitsuami`): one native window per platform (AppKit, WinUI 3, GTK 4 / Kirigami)
whose whole content is a `GpuSurface` that the existing wgpu + glyphon renderer presents
to. Everything bemtvi-gui got from winit must come from mitsuami, and where mitsuami
can't provide it yet, the gap gets closed **in mitsuami** rather than worked around in
bemtvi.

## What bemtvi-gui uses from winit, and where it comes from now

| winit | mitsuami | Gap? |
|---|---|---|
| `EventLoop` + `ApplicationHandler` | `App::new().open(|| Window …).run()` | — |
| `EventLoopProxy::send_event` (IO thread → UI) | `spawn_local` awaiting a channel; mitsuami's executor wakes the UI thread from any thread | — |
| `ControlFlow::WaitUntil` (flush / auto-scroll / scroll-slide timers) | `sleep` in `spawn_local` tasks | — |
| `Window` title + inner size, `.with_name` (Linux app id) | `Window::new("bemtvi").size(…)`, `App::id/name` | — |
| `CloseRequested` → `:qa` | `Window::on_close_request` | — |
| `event_loop.exit()` (`bemtvi_exit`) | close the window (`open` signal false); `App::run` returns when the last window closes | — |
| `instance.create_surface(window)`, `inner_size`, `scale_factor`, `Resized`, `ScaleFactorChanged` | `GpuSurface::on_ready(SurfaceHandle)` / `on_resize(SurfaceSize)` | — |
| `CursorMoved` / `MouseInput` / `MouseWheel` / `ModifiersChanged` | `SurfaceInput::{PointerMoved, Button, Scroll}` with modifiers (points × scale = pixels; scroll sign flipped) | — |
| `set_cursor(EwResize/NsResize/Default)` | `GpuSurface::cursor(Cursor::System(…))` | — |
| `KeyboardInput` `logical_key` (layout character, Shift/AltGr folded in) | — | **yes** |
| `key_without_modifiers()` (the layout's character for a chord: `<A-c>` on macOS, `<C-w>` on AZERTY) | — | **yes** |
| `Ime::Commit` (dead keys, CJK input methods) | — | **yes** |
| `set_ime_allowed` / `set_ime_cursor_area` | — | **yes** |
| `rfd` open/save dialogs (blocking, unparented) | `open_file` / `save_file` services (async, on the window) | — |
| `arboard` clipboard read | unchanged (not winit) | — |

So the one gap is **text input on a `GpuSurface`** — mitsuami's own docs list it as "Not
yet" (ARCHITECTURE.md §13.26). Its surface reports keys only by where they are on the
keyboard (`KeyCode`), which is right for a game or a VM but can't type.

## mitsuami: text input on a `GpuSurface`

API (core + widgets), following mitsuami's W3C model for keys:

- `GpuSurface::text_input(on)` / `Prop::TextInput(bool)`: takes text as a text field does.
  Off by default, since an input method turned on over a game would eat its keys.
- `SurfaceInput::Key` gains `text: Option<String>` (what the key typed, as the platform's
  text input gives it: Shift, Caps Lock, AltGr, Option and a dead key before it composed
  in; never control characters; `None` without `text_input`) and `unmodified: Option<char>`
  (the layout's character for the key with no modifiers — what a shortcut names it by).
  `SurfaceInput` stops being `Copy` (it carries a `String`).
- `SurfaceInput::Text(String)`: text an input method committed, which no single key typed.
- `GpuSurface::text_cursor(rect)` / `Prop::TextCursor(Option<Rect>)`: where the caret is,
  in points from the surface's top left, so the input method's candidate window opens there.
- Each backend, natively: WinUI pairs `WM_KEYDOWN` with the `WM_CHAR`s `TranslateMessage`
  queued for it (as winit does), `ToUnicodeEx` for `unmodified`, IMM32 for input methods;
  AppKit `NSTextInputClient` (`interpretKeyEvents:`, `charactersIgnoringModifiers`);
  GTK a `GtkIMMulticontext` on the area's key controller; Kirigami `QKeyEvent::text` and
  `inputMethodEvent`. Headless reports the US layout's characters.
- Tests in `crates/mitsuami/tests/gpu_surface.rs`, a story, and the docs (BACKENDS.md,
  ARCHITECTURE.md §13.26), per mitsuami's CLAUDE.md.

Verification here is Windows: headless + native WinUI tests, and the example driven by
hand. AppKit and GTK are type-checked from Windows (`--target aarch64-apple-darwin`,
`DOCS_RS=1 --target x86_64-unknown-linux-gnu`); the Kirigami C++ shim can't be compiled
here (no Qt). mitsuami's docs say so, as its rules ask.

## bemtvi-gui on mitsuami

- `run()` builds a mitsuami `App` with one window holding a full-size `GpuSurface`
  (`text_input(true)`, focused on open); the `App` state moves into an `Rc<RefCell<…>>`
  shared by the surface's handlers and the UI-thread tasks.
- The IO thread keeps its tokio runtime; `UserEvent`s go over a `tokio::sync::mpsc`
  channel drained by a `spawn_local` task (no proxy).
- Repaints are coalesced: `request_redraw()` sets a flag and queues one paint task; the
  scroll slide re-requests every 8 ms while it runs; the `timeoutlen` flush and the
  drag auto-scroll are `sleep` tasks re-armed by each event.
- Keys: named keys from `KeyCode`; typed text from `text` (fed through `encode_key`'s
  single-char path, so `<` and specials stay escaped); chords (Control / Alt, Option on
  macOS) from `unmodified`; AltGr is a key that typed text with Control+Alt held
  (Windows), so it types. IME commits (`SurfaceInput::Text`) go through `encode_text`.
- Native open/save dialogs move from `rfd` to mitsuami's (async, parented). `rfd` stays
  only for the ssh askpass helper, which runs before any window exists.
- Tier-1 tests (`tests/keys.rs`, `tests/mouse.rs`) move from winit types to mitsuami's.
- Docs: architecture.md's GUI section; the crate description.

## Phases

1. **mitsuami: text input on `GpuSurface`** — core, widgets, headless, test kit, all four
   backends, tests, docs. Committed in the mitsuami repo. *Pause for review.*
2. **bemtvi-gui on mitsuami** — the port above, depending on the local mitsuami. Committed
   in bemtvi. *Pause for review.*

## Open questions

- How bemtvi depends on mitsuami until a release has text input: crates.io has 1.1.0;
  the work needs the local tree. A `[patch.crates-io]` to the checkout works locally but
  not in CI.
- Linux packaging: GTK 4.10+ and libadwaita 1.4+ (or Qt 6.5 + Kirigami) become build and
  run-time requirements of the AppImage. Windows needs the Windows App Runtime 2.4+.
