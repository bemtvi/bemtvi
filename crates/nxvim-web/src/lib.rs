//! Fully client-side WebAssembly build of the nxvim editor.
//!
//! There is **no server**. The editor model — [`nxvim_core::Editor`], which holds
//! the buffers, modes, motions, operators, ex-commands, undo, search, and the
//! renderable `View` — is pure, synchronous, and depends only on pure-Rust crates,
//! so it compiles to `wasm32-unknown-unknown` and runs entirely in the browser. The
//! JS frontend (`web/index.html`) drives it through the thin [`WebEditor`] handle:
//! it sends keystrokes as vim key-notation, reads back the `View` as JSON to render
//! in HTML/CSS, and handles file open/save itself via the browser's File System
//! Access API — feeding file contents in with [`WebEditor::load_file`] and writing
//! [`WebEditor::buffer_text`] back out, the in-memory analogue of `:e`/`:w`.
//!
//! What's here is the editor *core* only: modal editing, the ex-command surface,
//! undo, search/substitute, multiple buffers, splits, and tab pages. The features
//! that live in `nxvim-server` — Lua config and LSP — are intentionally absent,
//! since a client-only build has no server to host them. Syntax highlighting is the
//! exception: rather than the server's native treesitter (which links C and can't
//! target wasm), the JS frontend layers its *own* client-side highlighter
//! (`web/highlight.js`) on top, parsing the buffer text with the WebAssembly build
//! of tree-sitter — so this Rust core stays untouched and still emits no highlight
//! data of its own.

use std::sync::{Arc, Mutex};

use nxvim_core::view::{ScrollAnim, Separator, View, WindowView};
use nxvim_core::{parse_keys, BorderStyle, Clipboard, Editor, MouseEvent};
use nxvim_view::{encode_paste, notation as view_notation, Key as ViewKey};
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

/// Map a neutral key `name` (from the JS frontend) to the shared [`ViewKey`]: a
/// single character, or one of the named keys the encoder understands. `None` for
/// an unrecognized name (a dead key, a modifier-only press).
pub(crate) fn neutral_key(name: &str) -> Option<ViewKey> {
    Some(match name {
        "Esc" => ViewKey::Esc,
        "CR" => ViewKey::Enter,
        "BS" => ViewKey::Backspace,
        "Tab" => ViewKey::Tab,
        "Del" => ViewKey::Delete,
        "Left" => ViewKey::Left,
        "Right" => ViewKey::Right,
        "Up" => ViewKey::Up,
        "Down" => ViewKey::Down,
        "Home" => ViewKey::Home,
        "End" => ViewKey::End,
        "PageUp" => ViewKey::PageUp,
        "PageDown" => ViewKey::PageDown,
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => ViewKey::Char(c),
                _ => return None,
            }
        }
    })
}

/// Encode one neutral key press (modifier flags + key `name`) into vim key-notation
/// through the **shared** `nxvim-view` encoder — the same contract the TUI/GUI clients
/// use. `None` for an unrecognized `name`. Used by [`WebEditor::key`] to feed the
/// notation to the local core.
pub(crate) fn key_notation(ctrl: bool, alt: bool, shift: bool, name: &str) -> Option<String> {
    let key = neutral_key(name)?;
    let mut notation = view_notation(ctrl, alt, key);
    // `nxvim_view::notation` is shift-agnostic — a character already carries its shifted
    // form, and the TUI/GUI add `S-` for *named* keys themselves. Do the same for the
    // common bare-shift case (e.g. `<S-Tab>`); a named key with shift *and* ctrl/alt is
    // rare enough to fall through unshifted.
    if shift && !ctrl && !alt && !matches!(key, ViewKey::Char(_)) {
        notation = format!("<S-{}", &notation[1..]);
    }
    Some(notation)
}

/// The clipboard register (`"+`/`"*`) for the browser. The core's [`Clipboard`]
/// contract is **synchronous** (it reads/writes a register inline during a yank or
/// paste), but the browser's `navigator.clipboard` is **async-only** — so this is a
/// cache the core touches synchronously, with the JS layer ferrying it to and from
/// the real system clipboard around each keystroke:
///
/// * yank (`"+y`) → [`Clipboard::set`] updates `value` and stages `pending`; the JS
///   layer drains `pending` via [`WebEditor::take_clipboard_write`] right after the
///   keystroke (still inside the browser's user-gesture window) and pushes it to
///   `navigator.clipboard.writeText`.
/// * paste (`"+p`) → [`Clipboard::get`] returns `value`; the JS layer keeps `value`
///   fresh by reading `navigator.clipboard.readText` (asynchronously, e.g. when a
///   register sequence begins) into [`WebEditor::set_clipboard_text`].
///
/// `Arc<Mutex<_>>` (not `Rc<RefCell<_>>`) because [`Clipboard`] is `Send`; on wasm
/// it never actually contends. Mirrors the test harness's `FakeClipboard`.
#[derive(Default)]
struct ClipState {
    /// What the synchronous core sees: the cached `(text, linewise)`, or `None` for
    /// an empty/unknown clipboard (then `"+p` is a no-op, as in the native clients).
    value: Option<(String, bool)>,
    /// Text just yanked to the clipboard, awaiting the JS layer's async push to the
    /// browser; drained by [`WebEditor::take_clipboard_write`].
    pending: Option<String>,
}

#[derive(Clone, Default)]
struct WebClipboard(Arc<Mutex<ClipState>>);

impl Clipboard for WebClipboard {
    fn get(&self) -> Option<(String, bool)> {
        self.0.lock().unwrap().value.clone()
    }

    fn set(&self, text: &str, linewise: bool) {
        let mut st = self.0.lock().unwrap();
        st.value = Some((text.to_string(), linewise));
        st.pending = Some(text.to_string());
    }
}

/// A browser-side handle to one editor. Constructed once per page; every keystroke,
/// command, file load, and repaint goes through it.
#[wasm_bindgen]
pub struct WebEditor {
    editor: Editor,
    width: usize,
    height: usize,
    /// Shared handle to the clipboard register's cache — the same `Arc` the editor
    /// holds as its [`Clipboard`] provider, so the JS-facing methods below can read
    /// and seed it.
    clip: WebClipboard,
    /// Fractional wheel remainder per axis `(horizontal, vertical)`, in whole-line
    /// units. A pixel-precise trackpad or a hi-res mouse wheel fires many small
    /// sub-line `wheel` deltas; without accumulation each one would emit a full
    /// `'mousescroll'` notch and the view would fly. The remainder carries between
    /// events so a slow drag still scrolls one row at a time. Mirrors the GUI
    /// client's `wheel_accum` (`nxvim-gui`). See [`WebEditor::wheel`].
    wheel_accum: (f32, f32),
}

#[wasm_bindgen]
impl WebEditor {
    /// Create a fresh editor sized to a `width` × `height` cell grid (the JS side
    /// computes these from the window size and the measured monospace cell).
    #[wasm_bindgen(constructor)]
    pub fn new(width: usize, height: usize) -> WebEditor {
        // Panics in wasm are otherwise silent; route them to the browser console.
        console_error_panic_hook();
        // Wire the clipboard register to a browser-backed cache. The editor owns a
        // boxed clone as its `Clipboard` provider; we keep a clone here so the
        // JS-facing methods can read/seed the same `Arc`.
        let clip = WebClipboard::default();
        let mut editor = Editor::new();
        editor.set_clipboard(Box::new(clip.clone()));
        WebEditor {
            editor,
            width: width.max(1),
            height: height.max(1),
            clip,
            wheel_accum: (0.0, 0.0),
        }
    }

    /// Drain the text the editor most recently yanked to the clipboard register
    /// (`"+y`/`"+d`/…), for the JS layer to push to the browser's async
    /// `navigator.clipboard.writeText`. `None` when nothing is pending. The JS side
    /// calls this right after each keystroke, while still inside the user-gesture
    /// window where a clipboard write is permitted.
    pub fn take_clipboard_write(&mut self) -> Option<String> {
        self.clip.0.lock().unwrap().pending.take()
    }

    /// Seed the clipboard register from the browser's system clipboard: the JS layer
    /// reads `navigator.clipboard.readText()` (asynchronously) and feeds the text
    /// here, so a later `"+p` pastes what was copied in another app. Linewise is
    /// re-derived from a trailing newline, exactly as the native clients do.
    pub fn set_clipboard_text(&mut self, text: &str) {
        let linewise = text.ends_with('\n');
        self.clip.0.lock().unwrap().value = Some((text.to_string(), linewise));
    }

    /// Update the cell grid size after a browser resize. The next `view_json`
    /// reflects the new viewport (the core re-lays-out and re-clamps scroll).
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.height = height.max(1);
    }

    /// Feed a vim key-notation string (`"i"`, `"<Esc>"`, `"<C-w>v"`, `"dd"`, …) —
    /// the same notation the TUI/GUI clients send as `nvim_input`. Each parsed key
    /// is applied to the editor in order. The JS side uses this only for synthetic
    /// input (the `:eo`/`:wo` flow aborts the command line with it); real keystrokes
    /// go through [`key`](Self::key).
    pub fn input(&mut self, notation: &str) {
        for key in parse_keys(notation) {
            self.editor.input(key);
        }
    }

    /// Encode one key press and feed it to the editor — going through the **shared**
    /// `nxvim-view` notation encoder, the same contract the TUI/GUI clients use, so
    /// the web client doesn't re-implement vim key-notation in JS. The JS frontend
    /// maps the browser `KeyboardEvent` to the modifier flags plus a neutral key
    /// `name` (a single character, or one of `Esc`/`CR`/`BS`/`Tab`/`Del`/`Left`/
    /// `Right`/`Up`/`Down`/`Home`/`End`/`PageUp`/`PageDown`) — exactly the mapping
    /// the winit/crossterm front ends do before calling `nxvim_view::notation`. An
    /// unrecognized `name` is ignored.
    pub fn key(&mut self, ctrl: bool, alt: bool, shift: bool, name: &str) {
        if let Some(notation) = key_notation(ctrl, alt, shift, name) {
            self.input(&notation);
        }
    }

    /// Encode a bracketed paste through the shared `nxvim-view` encoder and feed it
    /// (one shot, like the TUI/GUI), rather than re-deriving the escaping in JS.
    pub fn paste(&mut self, text: &str) {
        self.input(&encode_paste(text));
    }

    /// Run an ex-command directly (without the `:`), e.g. `"split"` or `"%s/a/b/g"`.
    /// The JS side uses this for the commands it triggers itself (window splits from
    /// a menu, etc.); ordinary `:`-commands the user types go through [`input`].
    pub fn command(&mut self, cmd: &str) {
        self.editor.command(cmd);
    }

    /// Forward a mouse gesture, exactly as the native clients forward
    /// `nvim_input_mouse`: `button` ∈ `left`/`right`/`middle`/`wheel`, `action` ∈
    /// `press`/`drag`/`release` (or `up`/`down`/`left`/`right` for the wheel),
    /// `modifier` a run of `C`/`S`/`A` (empty for none), at the **global** zero-based
    /// screen cell `(row, col)`. The editor owns the hit-test from a cell back to a
    /// window and buffer position, multi-click word/line selection, drag-select,
    /// wheel scroll, split-divider resize, and — in multi-cursor placement mode —
    /// toggling a cursor at the clicked cell. `stamp_ms` is a millisecond timestamp
    /// (the JS clock) the editor compares against `'mousetime'` for multi-click
    /// detection (it reads no clock itself). A malformed gesture is ignored.
    pub fn mouse(
        &mut self,
        button: &str,
        action: &str,
        modifier: &str,
        row: usize,
        col: usize,
        stamp_ms: f64,
    ) {
        if let Ok(mut ev) = MouseEvent::parse(button, action, modifier, row, col) {
            ev.stamp_ms = stamp_ms.max(0.0) as u64;
            self.editor.mouse(ev);
        }
    }

    /// Forward a browser `wheel` event, converting its raw `delta_x`/`delta_y` into
    /// whole scroll **notches** before handing them to the editor. The browser
    /// reports the delta in `delta_mode` units — `0` pixels, `1` lines, `2` pages —
    /// so pixels are divided by the cell size (`cell_w`/`cell_h`, the measured
    /// monospace cell in CSS px) to lines, and pages by the viewport. The fractional
    /// remainder is kept per axis ([`wheel_accum`](WebEditor)) so a pixel-precise
    /// trackpad or a hi-res wheel — which fire a burst of sub-line deltas — sum to
    /// the right distance instead of emitting a full `'mousescroll'` notch on every
    /// micro-event (which made the web view scroll far too fast). Each whole notch
    /// is one `wheel` gesture, capped per event so a flung wheel can't flood the
    /// core. The browser's positive `delta_y` scrolls the content up (a scroll
    /// *down*); positive `delta_x` scrolls right. This mirrors the GUI client
    /// (`nxvim-gui`'s `mouse_wheel`) exactly.
    #[allow(clippy::too_many_arguments)]
    pub fn wheel(
        &mut self,
        delta_x: f64,
        delta_y: f64,
        delta_mode: u32,
        cell_w: f64,
        cell_h: f64,
        modifier: &str,
        row: usize,
        col: usize,
        stamp_ms: f64,
    ) {
        let (lines_x, lines_y) = match delta_mode {
            1 => (delta_x as f32, delta_y as f32), // already line units
            2 => (
                delta_x as f32 * self.width.max(1) as f32,
                delta_y as f32 * self.height.saturating_sub(1).max(1) as f32,
            ),
            _ => (
                (delta_x / cell_w.max(1.0)) as f32,
                (delta_y / cell_h.max(1.0)) as f32,
            ),
        };
        let hnotch = drain_notches(lines_x, &mut self.wheel_accum.0);
        let vnotch = drain_notches(lines_y, &mut self.wheel_accum.1);

        // Cap the per-event repeat so a flung wheel (or a coarse page-mode delta)
        // can't flood the core with notches in one go.
        const MAX_STEPS: u32 = 10;
        if vnotch != 0 {
            let action = if vnotch > 0 { "down" } else { "up" };
            for _ in 0..vnotch.unsigned_abs().min(MAX_STEPS) {
                self.mouse("wheel", action, modifier, row, col, stamp_ms);
            }
        }
        if hnotch != 0 {
            let action = if hnotch > 0 { "right" } else { "left" };
            for _ in 0..hnotch.unsigned_abs().min(MAX_STEPS) {
                self.mouse("wheel", action, modifier, row, col, stamp_ms);
            }
        }
    }

    /// Load `contents` into the editor as the file named `name` — the browser open
    /// path. The bytes come from the File System Access API (or a file input), not a
    /// filesystem, so this is the in-memory analogue of `:e {name}`: reuse the empty
    /// buffer, replace its text, bind the name, and mark it unmodified.
    pub fn load_file(&mut self, name: &str, contents: &str) {
        let name = (!name.is_empty()).then(|| name.to_string());
        self.editor.load_str(name, contents);
    }

    /// The current buffer's full text, ready to write back to disk (the browser does
    /// the actual write via the File System Access API). Vim files end with a
    /// trailing newline, so the editable lines are joined and one is appended.
    pub fn buffer_text(&self) -> String {
        let mut text = self.editor.lines().join("\n");
        text.push('\n');
        text
    }

    /// Record that the current buffer was just saved as `name`: bind the name and
    /// clear the modified flag (the JS side calls this after a successful write).
    pub fn mark_saved(&mut self, name: &str) {
        let name = (!name.is_empty()).then(|| name.to_string());
        self.editor.mark_saved(name);
    }

    /// Project the editor's `View` for the current viewport and serialize it to the
    /// JSON the HTML renderer consumes. Called after every input/command/resize.
    pub fn view_json(&mut self) -> String {
        let view = self.editor.view(self.width, self.height);
        view_to_json(&view).to_string()
    }
}

/// Serialize a core [`View`] into the JSON shape `web/index.html` renders. Only the
/// fields the HTML renderer uses are included: no per-row highlight spans (syntax
/// highlighting is computed in the browser by `web/highlight.js` from the buffer
/// text, not projected here), and the status line is synthesized in JS from the
/// per-window facts rather than a `%`-format projection.
fn view_to_json(view: &View) -> Value {
    json!({
        "rows": view_rows(view),
        "cols": view_cols(view),
        "mode": view.mode_label,
        "command_mode": view.command_mode,
        "pending_replace": view.pending_replace,
        "cmdline": view.cmdline,
        "cmdline_prefix": view.cmdline_prefix.to_string(),
        "cmdline_prompt": view.cmdline_prompt,
        "cmdline_cursor": view.cmdline_cursor,
        "message": view.message,
        "windows": view.windows.iter().map(window_to_json).collect::<Vec<_>>(),
        "separators": view.separators.iter().map(separator_to_json).collect::<Vec<_>>(),
        "tabline": view.tabline.iter().map(|t| json!({
            "label": t.label, "modified": t.modified, "window_count": t.window_count,
        })).collect::<Vec<_>>(),
        "current_tab": view.current_tab,
        "panel": view.panel.as_ref().map(|p| json!({
            "title": p.title,
            "lines": p.lines,
            "cursor_row": p.cursor_row,
            "cursor_span": p.cursor_span,
            "height": p.height,
        })),
    })
}

/// Total grid rows the windows lay out within (windows area + tabline + command
/// row), derived from the largest window extent — so the JS renderer can size its
/// regions identically to the core's layout.
fn view_rows(view: &View) -> usize {
    let wins = view
        .windows
        .iter()
        .map(|w| w.rect.y + w.rect.height)
        .max()
        .unwrap_or(0);
    let tabline = usize::from(!view.tabline.is_empty());
    let panel = view.panel.as_ref().map_or(0, |p| p.height + 1);
    // windows area + the tabline row + the panel + the single command row.
    wins + tabline + panel + 1
}

fn view_cols(view: &View) -> usize {
    view.windows
        .iter()
        .map(|w| w.rect.x + w.rect.width)
        .max()
        .unwrap_or(0)
}

/// Add `amount` line-notches to the running `accum` and return the whole notches
/// to emit now, leaving the sub-line remainder in `accum`. A wheel mouse sends
/// roughly one line per detent (emitted at once); a trackpad sends many fractional
/// lines that accumulate until they cross a whole line, so a slow drag still
/// scrolls one row at a time rather than not at all. Truncation is toward zero, so
/// the scroll direction never flips from rounding. Mirrors `nxvim-gui`'s
/// `drain_notches`. See [`WebEditor::wheel`].
fn drain_notches(amount: f32, accum: &mut f32) -> i32 {
    *accum += amount;
    let whole = accum.trunc();
    *accum -= whole;
    whole as i32
}

fn window_to_json(w: &WindowView) -> Value {
    json!({
        "rect": { "x": w.rect.x, "y": w.rect.y, "width": w.rect.width, "height": w.rect.height },
        "focused": w.focused,
        "floating": w.floating,
        "border": border_name(w.border),
        "title": w.title,
        "lines": w.lines,
        "cursor_row": w.cursor_row,
        "cursor_col": w.cursor_col,
        "cursor_screen_col": w.cursor_screen_col,
        "cursor_line": w.cursor_line,
        "leftcol": w.leftcol,
        "secondary_cursors": w.secondary_cursors.iter().map(|&(r, c)| json!([r, c])).collect::<Vec<_>>(),
        "selection": spans_opt(&w.selection),
        "secondary_selection": spans_rows(&w.secondary_selection),
        "search": spans_rows(&w.search),
        "incsearch": spans_opt(&w.incsearch),
        "scroll": w.scroll.as_ref().map(scroll_to_json),
        "numbers": w.numbers.iter().map(|n| match n {
            Some(n) => json!(n),
            None => Value::Null,
        }).collect::<Vec<_>>(),
        "number": w.number,
        "relativenumber": w.relativenumber,
        "number_width": w.number_width,
        "tabstop": w.tabstop,
        "file_name": w.file_name,
        "filetype": w.filetype,
        "unnamed": w.unnamed,
        "modified": w.modified,
    })
}

fn separator_to_json(s: &Separator) -> Value {
    json!({ "vertical": s.vertical, "x": s.x, "y": s.y, "length": s.length })
}

/// A focused window's scroll gesture: the slide's endpoints plus the self-contained
/// band (`base_line` + aligned `lines`/`numbers`/`selection`) the JS interpolates
/// over per frame. Mirrors [`ScrollAnim`]; the browser highlights band rows by their
/// 1-based `numbers`, so no per-row syntax data is projected here.
fn scroll_to_json(s: &ScrollAnim) -> Value {
    json!({
        "from_top": s.from_top,
        "to_top": s.to_top,
        "from_cursor": s.from_cursor,
        "to_cursor": s.to_cursor,
        "duration_ms": s.duration_ms,
        "base_line": s.base_line,
        "lines": s.lines,
        "selection": spans_opt(&s.selection),
        // Search matches ride the band so `hlsearch`/`incsearch` keep highlighting
        // the moving text instead of vanishing until the slide settles.
        "search": spans_rows(&s.search),
        "incsearch": spans_opt(&s.incsearch),
        "numbers": s.numbers.iter().map(|n| match n {
            Some(n) => json!(n),
            None => Value::Null,
        }).collect::<Vec<_>>(),
    })
}

/// A per-row list of optional `[start, end]` spans (selection / incsearch): each
/// row is `[s, e]` or `null`.
fn spans_opt(rows: &[Option<(usize, usize)>]) -> Vec<Value> {
    rows.iter()
        .map(|span| match span {
            Some((s, e)) => json!([s, e]),
            None => Value::Null,
        })
        .collect()
}

/// A per-row list of span lists (search matches / secondary selections): each row
/// is an array of `[start, end]` pairs.
fn spans_rows(rows: &[Vec<(usize, usize)>]) -> Vec<Value> {
    rows.iter()
        .map(|row| Value::Array(row.iter().map(|&(s, e)| json!([s, e])).collect()))
        .collect()
}

/// The float border as a lowercase name the JS renderer maps to box-drawing glyphs;
/// `None` for a borderless float or a tiled window.
fn border_name(border: BorderStyle) -> Option<&'static str> {
    match border {
        BorderStyle::None => None,
        BorderStyle::Single => Some("single"),
        BorderStyle::Rounded => Some("rounded"),
        BorderStyle::Double => Some("double"),
        BorderStyle::Solid => Some("solid"),
    }
}

/// Forward Rust panics to the browser console so a wasm trap isn't silent. A tiny
/// inline version of the `console_error_panic_hook` crate (avoids the dependency):
/// set once, it formats each panic and logs it via `console.error`.
pub(crate) fn console_error_panic_hook() {
    use std::sync::Once;
    static SET: Once = Once::new();
    SET.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            log_error(&info.to_string());
        }));
    });
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn log_error(msg: &str);
}
