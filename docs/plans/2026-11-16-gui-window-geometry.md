# bemtvi-gui remembers its window: position, size, maximized

**Goal.** `bemtvi-gui` opens its window where, how large, and whether maximized it was
when it last closed — or as a config says:

| Option          | Values                                        | Default    |
|-----------------|-----------------------------------------------|------------|
| `'guimaximize'` | `remember`, `always`, `never`                 | `remember` |
| `'guiposition'` | `remember`, `centered`                        | `remember` |
| `'guisize'`     | `remember`, `{W}%x{H}%` (of the screen's work area) | `remember` |

`'guimaximize'` is done (uncommitted; it needed the mitsuami WinUI fix for a window
opening maximized, see *Status*). This plan adds the other two.

## Decisions (agreed 2026-11-16)

- **mitsuami grows a saved-frame API**, the platforms' own save/restore model (Win32
  `GetWindowPlacement`/`SetWindowPlacement`, AppKit's saved frame, Qt's
  `saveGeometry`), rather than a raw position signal every app would have to clamp
  onto screens itself.
- **`'guisize'` defaults to `remember`**; with nothing saved yet (first launch) the
  window opens at today's 960 × 640 points. `{W}%x{H}%` opens it at that fraction of
  the work area every time.
- **`'guiposition'` defaults to `remember`**; with nothing saved yet the platform
  places it (what it does today). `centered` centres it on its screen every time.
- **Both apply live**, as `guimaximize=always` does: `:set guiposition=centered`
  centres the window now, `:set guisize=50%x50%` resizes it now. Setting `remember`
  changes nothing until the next launch.
- **Wayland**: no client can read or set its window's position. Size and maximized
  are still remembered; setting `'guiposition'` to `centered` there says once, in the
  message line, that the compositor places windows (no silent no-op).

## The mitsuami API (phase 1)

All in mitsuami's own terms, every backend (its CLAUDE.md: *Adding a widget*-style,
touching core, widgets, headless, test kit, the four backends, tests, `BACKENDS.md`,
`ARCHITECTURE.md` §14).

- **`WindowFrame { position: Option<Point>, size: Size }`** — a window's *restored*
  frame: where and how large it is when not maximized or in full screen, also while
  it is (Win32's `rcNormalPosition`). `size` is the content size in points, what
  `Window::size` takes. `position` is the window's top-left in the platform's own
  desktop coordinates: a value to hand back to the same platform, nothing more.
  `None` where the platform can't tell (Wayland).
- **`Window::frame(Signal<Option<WindowFrame>>)`** — when it opens, a `Some` places
  and sizes it there (a position on no screen any more is moved onto the nearest
  one, a size larger than the work area shrinks to it — the backend's job, as the
  platforms' own restore does); while open, the user moving or resizing it updates
  the signal (`UiEvent::FrameChanged`, absorbed as `Prop::Frame`), and the app setting
  it moves/resizes it. Maximizing doesn't change it.
- **`Window::placement(Placement)`**, `Placement::{System, Centred}` — where it opens
  when no frame says: the platform's choice, or centred on its screen. Read at each
  opening. Replaces WinUI's process-wide `set_window_placement` (kept, deprecated, as
  the default for windows that don't say).
- **`WindowSize::Screen { width, height }`** — fractions of the work area of the
  screen it opens on.
- **`Ui::centre_window(window)`** and **`Ui::work_area(window) -> Option<Rect>`** —
  the live halves: centre it now; the work area of the screen it's on (to size it to
  a fraction now, with the existing `Ui::set_window_size`).

Per platform: WinUI — `AppWindow.Move`/`Resize` + `GetWindowPlacement` for the
restored rect; AppKit — `setFrame:`/`center`/`frame` (restored while zoomed via
`NSWindow`'s user state), `visibleFrame`; GTK 4 — size only (`default-width/height`,
no position API at all; position `None`), monitor work area from
`gdk_display_get_monitor_at_surface`; Kirigami — `QWindow::setPosition`/`geometry`
on X11, position `None` on Wayland; headless — simulated, with a settable work area.

Only WinUI and headless run here; AppKit, GTK and Kirigami type-check from macOS /
CI. Say so in `ARCHITECTURE.md`.

## bemtvi (phase 2)

- **Options** `'guiposition'`, `'guisize'`: core field + `set_scalar`/get + catalog +
  loud `E474` validation (`guisize` parses `remember` or `{1-100}%x{1-100}%`), Lua
  mirror and `state.lua`, redraw relay, `View` fields — as `'guimaximize'` did.
- **Client** (`bemtvi-gpu`): `Effect::WindowPlacement(..)` / `Effect::WindowSize(..)`
  on a change (first value counts, kept across `:connect`), like
  `Effect::WindowMaximize`; `bemtvi-mitsuami`: `Editor::on_window_geometry` (or the
  two callbacks), passing it to the host.
- **`bemtvi-gui`**: the state file becomes `key=value` lines (`maximized`, `x`, `y`,
  `width`, `height`, plus the resolved `position`/`size` modes so a non-`remember`
  mode opens right before the config runs). Open with `Window::frame` /
  `placement` / `size`; on the effect, apply live (`Ui::centre_window`, work area →
  `set_window_size`); save at exit. On Wayland (frame position `None`),
  `guiposition=centered` reports once.
- **Tests**: server option round-trip + rejection; gpu client effects fire on change
  only; `crates/bemtvi-gui/tests/window_state.rs` e2e (`--ignored`) extended to
  position and size; example `examples/gui-window/` with its own spec.

## Phase 2b — mitsuami: WinUI windows open with the system's opening animation

**Problem.** No mitsuami WinUI window plays Windows' opening animation, restored or
maximized: it appears in one frame (closing does animate). bemtvi-gui opening
maximized shows this most, and the `f6d7db0` change (maximize while invisible,
transitions off) only traded the grow-from-small animation for none at all.

**Cause (measured).** A screen-capture probe sampling ~30 fps (a 64 px patch at the
screen's centre and one near its corner, mean difference from before launch) shows:

- a plain WinForms window: first show fades in over 3 frames, hide fades out over 4,
  **a second show jumps in with no animation** — Windows animates a window's *first*
  show only;
- mitsuami creates every window activated (`Window.Activate()`), layered at alpha 1,
  "because XAML only measures elements in a live tree", and reveals it later by
  dropping the layering: the first show, and its animation, happen invisibly at
  creation. Hiding and re-showing at reveal, `SW_SHOWMAXIMIZED`, or cloaking
  (`DWMWA_CLOAK`) and uncloaking instead of layering: all still one frame (measured).
- **Not activating at creation works**: the content still loads (`Loaded` fires with
  the current Windows App SDK), and activating at reveal (`SW_SHOWMAXIMIZED` for a
  window opening maximized) fades in over 4+ frames, restored or maximized, and the
  saved frame and maximized state survive (measured; the first-restore glitch of the
  hide-and-show attempt was a symptom of a second show).

**Change** (`mitsuami-winui`, real windows only — `show_windows`):

1. `new_window`: don't `Activate()` a window that will be shown; wait for `Loaded`
   as now. Test windows (`show_windows = false`) keep today's path (activated,
   alpha 1): XAML stops rendering windows it considers hidden, and captures need
   rendering.
2. `show_pending_windows`: the reveal is the window's first show — `Activate()`, or
   `ShowWindow(SW_SHOWMAXIMIZED)` for one asked to open maximized (and drop the
   `f6d7db0` transitions dance). Keep placing it, modality, focus restore, `Present`
   on a not-yet-shown window, then report its frame.
3. Check what assumed activation at creation: the reveal's focus comment ("activated
   when made"), `WindowActiveChanged` (now first reported at reveal), keyboard focus
   going to `autofocus` content right after reveal, modal windows (`make_modal` at
   show), popovers, the tray's `Present`, and a window measured before it's ever
   shown (text, toolbars, sidebar) laying out at the same sizes as before.

**Verification.** mitsuami's native suite (test windows keep their path), plus the
probe for the animation, made a `--ignored` e2e in bemtvi-gui (fade-in over several
frames, restored and maximized) so it can't regress silently; typing into the editor
right after it opens; the first restore animating; the window-state e2e tests. By eye
on macOS/Linux isn't needed: WinUI only.

## Phase 3

Release mitsuami with phase 1 (and the WinUI maximize fix), drop bemtvi's temporary
`[patch.crates-io]`, raise the `mitsuami`/`mitsuami-test` floor.

## Status

- [x] `'guimaximize'` + the mitsuami WinUI fix for opening maximized (uncommitted).
- [x] Phase 1 — mitsuami saved frame / placement / screen size (uncommitted, in
      `C:\Users\david\work\mitsuami`). Headless and WinUI run natively; AppKit, GTK
      and Kirigami type-check (Kirigami's new C++ `mq_screen_available_at` isn't
      compiled here). `Ui::work_area` takes `Option<NodeId>` (`None`: the screen a
      new window opens on).
- [x] Phase 2 — bemtvi options and the GUI's state. The two callbacks are
      `Editor::on_window_position` / `on_window_sizing` (beside
      `on_window_maximize`). A share applied live measures the window's own screen;
      at the next launch, before the window exists, it's the primary display's work
      area less a title bar's estimate, so the two can differ by a few points. A
      maximized window isn't resized live: the share is how it opens next time.
      `examples/gui-maximize` became `examples/gui-window` (all three options).
- [x] Phase 2b — WinUI windows open with the system's opening animation (mitsuami
      `bfdade3`; `crates/bemtvi-gui/tests/window_opening.rs` fails before it and
      passes after). Typing right after it opens couldn't be checked here: from a
      background session Windows gives the window no foreground, and that test
      skips rather than type into another window. To check by eye.
- [x] Three rounds of review on both branches, findings fixed; both squash-merged to
      their `main`s (mitsuami `f2a0a86`), bemtvi's `[patch]` pinned to that commit.
- [ ] Phase 3 — release mitsuami and unpatch.
