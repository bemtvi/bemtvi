//! The **remote-mode** client: a browser-side handle that speaks the server's own
//! msgpack-RPC over a Socket.IO transport, instead of running the editor locally.
//!
//! Where [`WebEditor`](crate::WebEditor) *is* the editor (the serverless build), this is a
//! thin client of a real, full-featured remote `nxvim-server` (Lua + treesitter + LSP). It
//! is the wasm sibling of the TUI/GUI clients, and reuses the exact same pieces they do:
//!
//! - **Decode** — the server pushes a `redraw` notification (a full frame) carrying one
//!   msgpack map; [`nxvim_view::View::update`] decodes it into the rich client model
//!   (styles, highlights, chrome, diagnostics, pmenu, …). [`RemoteClient::view_json`] then
//!   serializes that for the HTML renderer.
//! - **Encode** — keystrokes go through the shared [`crate::key_notation`] /
//!   [`nxvim_view::encode_paste`] encoders, then are wrapped as `nvim_input` /
//!   `nvim_input_mouse` / … RPC frames.
//!
//! The transport itself lives in JS: the page owns the Socket.IO socket and ferries raw
//! binary payloads in (`feed`) and out (the `Vec<u8>` each method returns). The framing —
//! reassembling msgpack frames from arbitrary byte chunks — is done here, mirroring
//! `nxvim_rpc::reader_task` but **synchronously** (tokio IO doesn't target wasm), so the
//! client never depends on a runtime.

use std::io::{self, Cursor};

use nxvim_view::{
    encode_paste, DiagSign, DiagSpan, DiagVirt, HlSpan, InlayHint, PmenuItem, StatusSegment, Style,
    View,
};
use rmpv::Value;
use serde_json::{json, Value as Json};
use wasm_bindgen::prelude::*;

use crate::key_notation;

/// Largest single not-yet-complete frame we buffer before concluding the peer is sending
/// garbage and tearing the connection down. Kept equal to `nxvim_rpc::MAX_FRAME` so any
/// frame the server is willing to send is one we accept.
const MAX_FRAME: usize = 64 * 1024 * 1024;
/// Maximum msgpack container nesting we decode — equal to `nxvim_rpc::MAX_DEPTH`. rmpv's
/// decoder is recursive; this cap surfaces a deeply-nested frame as a clean error instead
/// of overflowing the stack.
const MAX_DEPTH: usize = 128;

/// A browser-side client of a remote `nxvim-server`. Constructed once per page (in remote
/// mode); the JS layer pumps Socket.IO binary payloads through [`feed`](Self::feed) and
/// sends the `Vec<u8>` the encoder methods return.
#[wasm_bindgen]
pub struct RemoteClient {
    /// The latest decoded frame — replaced wholesale on each `redraw` (the server sends
    /// full frames, never deltas).
    view: View,
    /// Partial-frame accumulator: raw bytes received but not yet forming a complete
    /// msgpack value. Mirrors `nxvim_rpc::reader_task`'s `buf`.
    inbuf: Vec<u8>,
    /// Next request msgid. Only `nvim_ui_attach` is a request; the counter just keeps ids
    /// unique (the client never awaits a response through this type).
    next_id: u64,
    width: usize,
    height: usize,
    /// Set when a `redraw` landed since the last [`view_json`](Self::view_json), so JS can
    /// skip re-rendering a frame that carried only responses / other notifications.
    dirty: bool,
    /// Set once the stream is structurally dead (a corrupt frame, or the buffer grew past
    /// [`MAX_FRAME`]); JS should close the socket.
    closed: bool,
    /// A server→client request whose id is owed a response. The server rarely requests
    /// anything, but if it does and we never reply it would hang — so we mirror the TUI's
    /// blanket `Ok(Nil)` reply via [`take_response`](Self::take_response).
    owed_response: Option<u64>,
}

#[wasm_bindgen]
impl RemoteClient {
    /// Create a client sized to a `width` × `height` cell grid (the JS side computes these
    /// from the window size and the measured monospace cell, exactly as for `WebEditor`).
    #[wasm_bindgen(constructor)]
    pub fn new(width: usize, height: usize) -> RemoteClient {
        // Panics in wasm are otherwise silent; route them to the browser console (shared
        // with `WebEditor`).
        crate::console_error_panic_hook();
        RemoteClient {
            view: View::default(),
            inbuf: Vec::with_capacity(8192),
            next_id: 1,
            width: width.max(1),
            height: height.max(1),
            dirty: false,
            closed: false,
            owed_response: None,
        }
    }

    // ---- outgoing: each returns the encoded RPC frame for the JS layer to `socket.emit` ----

    /// `nvim_ui_attach(width, height, {})` — the first frame, sent on connect. A request
    /// (matching the TUI), though the client doesn't await its reply.
    pub fn attach(&mut self) -> Vec<u8> {
        self.request(
            "nvim_ui_attach",
            vec![
                Value::from(self.width as u64),
                Value::from(self.height as u64),
                Value::Map(vec![]),
            ],
        )
    }

    /// Wrap a vim key-notation string as one `nvim_input` notification.
    pub fn input(&self, notation: &str) -> Vec<u8> {
        notify("nvim_input", vec![Value::from(notation)])
    }

    /// Encode one key press (the same neutral mapping `WebEditor::key` uses) as
    /// `nvim_input`. An unrecognized `name` yields an empty `Vec` (JS sends nothing).
    pub fn key(&self, ctrl: bool, alt: bool, shift: bool, name: &str) -> Vec<u8> {
        match key_notation(ctrl, alt, shift, name) {
            Some(notation) => self.input(&notation),
            None => Vec::new(),
        }
    }

    /// Encode a bracketed paste through the shared encoder and wrap it as `nvim_input`.
    pub fn paste(&self, text: &str) -> Vec<u8> {
        self.input(&encode_paste(text))
    }

    /// `nvim_input_mouse(button, action, modifier, grid=0, row, col)` — the same gesture
    /// the TUI/GUI forward; the server owns the hit-test and (remote) the multi-click
    /// clock, so no timestamp is sent.
    pub fn input_mouse(
        &self,
        button: &str,
        action: &str,
        modifier: &str,
        row: usize,
        col: usize,
    ) -> Vec<u8> {
        notify(
            "nvim_input_mouse",
            vec![
                Value::from(button),
                Value::from(action),
                Value::from(modifier),
                Value::from(0u64),
                Value::from(row as u64),
                Value::from(col as u64),
            ],
        )
    }

    /// `nvim_command(cmd)` — run an ex-command directly (menu splits, etc.). Ordinary
    /// `:`-commands the user types reach the server as keystrokes via [`input`](Self::input).
    pub fn command(&self, cmd: &str) -> Vec<u8> {
        notify("nvim_command", vec![Value::from(cmd)])
    }

    /// `nvim_ui_try_resize(w, h)` — and update the cached size so a later `attach`/reconnect
    /// uses the current grid.
    pub fn try_resize(&mut self, width: usize, height: usize) -> Vec<u8> {
        self.width = width.max(1);
        self.height = height.max(1);
        notify(
            "nvim_ui_try_resize",
            vec![
                Value::from(self.width as u64),
                Value::from(self.height as u64),
            ],
        )
    }

    /// `nxvim_input_flush` — the `timeoutlen` idle flush, fired by a JS timer after a quiet
    /// gap, so a pending mapping prefix resolves (mirrors the TUI's armed flush).
    pub fn flush(&self) -> Vec<u8> {
        notify("nxvim_input_flush", vec![])
    }

    // ---- incoming ----

    /// Hand raw Socket.IO binary payload bytes in. Appends them to the buffer, then drains
    /// every complete msgpack frame — updating the view on `redraw`. Chunks may split or
    /// merge frames freely; partial frames are held until the rest arrives.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.inbuf.extend_from_slice(bytes);
        loop {
            let parsed = {
                let mut cur = Cursor::new(&self.inbuf[..]);
                match rmpv::decode::read_value_with_max_depth(&mut cur, MAX_DEPTH) {
                    Ok(v) => Some((v, cur.position() as usize)),
                    // A short read means the frame isn't fully buffered yet — wait for more.
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
                    // Any other decode error: the stream is structurally corrupt and the
                    // leading bytes will never decode. Tear it down.
                    Err(_) => {
                        self.closed = true;
                        return;
                    }
                }
            };
            match parsed {
                Some((val, n)) => {
                    self.inbuf.drain(..n);
                    self.dispatch(val);
                }
                None => break,
            }
        }
        // A frame that grows past the cap without completing is garbage or abusively large.
        if self.inbuf.len() > MAX_FRAME {
            self.closed = true;
        }
    }

    /// Whether a `redraw` arrived since the last [`view_json`](Self::view_json) call.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the stream is dead (corrupt frame or buffer overflow); JS should close.
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// Take the pending reply to a server→client request, if one is owed: an encoded
    /// `[1, id, Nil, Nil]` response. `None` when nothing is owed. JS calls this after each
    /// [`feed`](Self::feed) and emits the bytes if present — the blanket `Ok(Nil)` the TUI
    /// also sends, so an unexpected server request can't hang the session.
    pub fn take_response(&mut self) -> Option<Vec<u8>> {
        let id = self.owed_response.take()?;
        Some(encode(&Value::Array(vec![
            Value::from(1u64),
            Value::from(id),
            Value::Nil,
            Value::Nil,
        ])))
    }

    /// Serialize the latest decoded [`View`] into the JSON the HTML renderer consumes —
    /// the rich, server-styled shape (see [`rich_view_to_json`]). Clears the dirty flag.
    pub fn view_json(&mut self) -> String {
        self.dirty = false;
        rich_view_to_json(&self.view).to_string()
    }
}

impl RemoteClient {
    /// Encode a request `[0, id, method, params]`, bumping the msgid counter.
    fn request(&mut self, method: &str, params: Vec<Value>) -> Vec<u8> {
        let id = self.next_id;
        self.next_id += 1;
        encode(&Value::Array(vec![
            Value::from(0u64),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]))
    }

    /// Act on one decoded incoming frame.
    fn dispatch(&mut self, val: Value) {
        let Value::Array(mut arr) = val else {
            return; // ignore malformed frames
        };
        match arr.first().and_then(Value::as_u64) {
            // Notification: only `redraw` carries view state. Move the params array out
            // rather than cloning it — a redraw map is large.
            Some(2) => {
                if arr.get(1).and_then(Value::as_str) == Some("redraw") {
                    if let Value::Array(params) = take(&mut arr, 2) {
                        self.view.update(&params);
                        self.dirty = true;
                    }
                }
            }
            // Response to a request we sent: the client never awaits one — drop it.
            Some(1) => {}
            // Request from the server: owe it a blanket `Ok(Nil)` reply (see `take_response`).
            Some(0) => {
                if let Some(id) = arr.get(1).and_then(Value::as_u64) {
                    self.owed_response = Some(id);
                }
            }
            _ => {}
        }
    }
}

/// Encode a notification `[2, method, params]`.
fn notify(method: &str, params: Vec<Value>) -> Vec<u8> {
    encode(&Value::Array(vec![
        Value::from(2u64),
        Value::from(method),
        Value::Array(params),
    ]))
}

/// msgpack-encode a value into a fresh `Vec` (infallible to a `Vec`).
fn encode(val: &Value) -> Vec<u8> {
    let mut buf = Vec::with_capacity(256);
    rmpv::encode::write_value(&mut buf, val).expect("msgpack encoding cannot fail to a Vec");
    buf
}

/// Move element `idx` out of `arr`, leaving `Nil` in its place (`Nil` when out of bounds).
fn take(arr: &mut [Value], idx: usize) -> Value {
    arr.get_mut(idx)
        .map(|v| std::mem::replace(v, Value::Nil))
        .unwrap_or(Value::Nil)
}

// ----------------------------------------------------------------------------------------
// Rich View → JSON
//
// A superset of `crate::view_to_json` (the serverless shape): it adds the server-only
// fields a real server projects — the resolved style palette, per-row highlight /
// diagnostic / inlay-hint spans, chrome region styles, segment-based status lines, and the
// completion pmenu. The renderer keys on the presence of `styles` to choose the
// server-styled paint path over its own client-side highlighter.
// ----------------------------------------------------------------------------------------

/// Serialize a decoded [`View`] into the renderer's JSON shape.
fn rich_view_to_json(view: &View) -> Json {
    json!({
        "mode": view.mode_label,
        "command_mode": view.command_mode,
        "pending_replace": view.pending_replace,
        "cmdline": view.cmdline,
        "cmdline_prefix": view.cmdline_prefix.to_string(),
        "cmdline_prompt": view.cmdline_prompt,
        "cmdline_cursor": view.cmdline_cursor,
        "message": view.message,
        "styles": view.styles.iter().map(style_to_json).collect::<Vec<_>>(),
        "chrome": {
            "normal": opt_style(&view.normal),
            "line_nr": opt_style(&view.line_nr),
            "cursor_line_nr": opt_style(&view.cursor_line_nr),
            "visual": opt_style(&view.visual),
            "search": opt_style(&view.search_style),
            "incsearch": opt_style(&view.incsearch_style),
            "status_line": opt_style(&view.status_line),
            "end_of_buffer": opt_style(&view.end_of_buffer),
        },
        "global_status": segments_to_json(&view.global_status),
        "windows": view.windows.iter().map(window_to_json).collect::<Vec<_>>(),
        "separators": view.separators.iter().map(|s| json!({
            "vertical": s.vertical, "x": s.x, "y": s.y, "length": s.length,
        })).collect::<Vec<_>>(),
        "tabline": view.tabline.iter().map(|t| json!({
            "label": t.label, "modified": t.modified, "window_count": t.window_count,
        })).collect::<Vec<_>>(),
        "tabline_segments": segments_to_json(&view.tabline_segments),
        "current_tab": view.current_tab,
        "panel": view.panel.as_ref().map(|p| json!({
            "title": p.title, "lines": p.lines, "cursor_row": p.cursor_row,
            "cursor_span": p.cursor_span, "height": p.height,
        })),
        "pmenu": view.pmenu.as_ref().map(|p| json!({
            "items": p.items.iter().map(pmenu_item_to_json).collect::<Vec<_>>(),
            "selected": p.selected,
            "row": p.row, "col": p.col, "width": p.width, "height": p.height,
            "doc": p.doc,
        })),
    })
}

fn window_to_json(w: &nxvim_view::WindowView) -> Json {
    json!({
        "rect": w.rect.map(|r| json!({ "x": r.x, "y": r.y, "width": r.width, "height": r.height })),
        "focused": w.focused,
        "floating": w.floating,
        "border": border_name(w.border),
        "title": w.title,
        "lines": w.lines,
        "cursor_row": w.cursor_row,
        "cursor_screen_col": w.cursor_screen_col,
        "cursor_line": w.cursor_line,
        "leftcol": w.leftcol,
        "tabstop": w.tabstop,
        "secondary_cursors": w.secondary_cursors.iter().map(|&(r, c)| json!([r, c])).collect::<Vec<_>>(),
        "selection": opt_spans(&w.selection),
        "secondary_selection": multi_spans(&w.secondary_selection),
        "search": multi_spans(&w.search),
        "incsearch": opt_spans(&w.incsearch),
        "highlights": highlights_to_json(&w.highlights),
        "diagnostics": diagnostics_to_json(&w.diagnostics),
        "diagnostics_virt": diag_virt_to_json(&w.diagnostics_virt),
        "diagnostics_signs": diag_signs_to_json(&w.diagnostics_signs),
        "sign_column": w.sign_column,
        "inlay_hints": inlay_hints_to_json(&w.inlay_hints),
        "numbers": w.numbers.iter().map(|n| match n {
            Some(n) => json!(n),
            None => Json::Null,
        }).collect::<Vec<_>>(),
        "number": w.number,
        "relativenumber": w.relativenumber,
        "number_width": w.number_width,
        "status": segments_to_json(&w.status),
        "status_visible": w.status_visible,
        "unnamed": w.unnamed,
    })
}

/// A resolved [`Style`] → `{ fg, bg, sp, <attrs> }`, colors as `"#rrggbb"` strings (or
/// `null`) so the renderer can drop them straight into CSS.
fn style_to_json(s: &Style) -> Json {
    json!({
        "fg": hex(s.fg),
        "bg": hex(s.bg),
        "sp": hex(s.sp),
        "bold": s.bold,
        "italic": s.italic,
        "underline": s.underline,
        "undercurl": s.undercurl,
        "strikethrough": s.strikethrough,
        "reverse": s.reverse,
    })
}

fn opt_style(s: &Option<Style>) -> Json {
    s.as_ref().map_or(Json::Null, style_to_json)
}

/// `0xRRGGBB` → `"#rrggbb"`; `None` → JSON null. The low three bytes only (the wire never
/// sets the top byte).
fn hex(color: Option<u32>) -> Json {
    match color {
        Some(c) => Json::String(format!("#{:06x}", c & 0xff_ffff)),
        None => Json::Null,
    }
}

/// `(text, Option<Style>)` segments → `[{ text, style }]` (style is inline-resolved or null).
fn segments_to_json(segs: &[StatusSegment]) -> Vec<Json> {
    segs.iter()
        .map(|(text, style)| json!({ "text": text, "style": opt_style(style) }))
        .collect()
}

fn pmenu_item_to_json(item: &PmenuItem) -> Json {
    let (label, kind, detail) = item;
    json!([label, kind, detail])
}

/// Per-row `[start, end] | null` spans (selection / incsearch).
fn opt_spans(rows: &[Option<(u16, u16)>]) -> Vec<Json> {
    rows.iter()
        .map(|span| match span {
            Some((s, e)) => json!([s, e]),
            None => Json::Null,
        })
        .collect()
}

/// Per-row arrays of `[start, end]` pairs (search matches / secondary selections).
fn multi_spans(rows: &[Vec<(u16, u16)>]) -> Vec<Json> {
    rows.iter()
        .map(|row| Json::Array(row.iter().map(|&(s, e)| json!([s, e])).collect()))
        .collect()
}

/// Per-row highlight spans `[start, end, group, style_id | null]` (style_id indexes the
/// global `styles` palette).
fn highlights_to_json(rows: &[Vec<HlSpan>]) -> Vec<Json> {
    rows.iter()
        .map(|row| {
            Json::Array(
                row.iter()
                    .map(|(s, e, group, id)| json!([s, e, group, id]))
                    .collect(),
            )
        })
        .collect()
}

/// Per-row diagnostic underline spans `[start, end, severity, style_id | null]`.
fn diagnostics_to_json(rows: &[Vec<DiagSpan>]) -> Vec<Json> {
    rows.iter()
        .map(|row| {
            Json::Array(
                row.iter()
                    .map(|(s, e, sev, id)| json!([s, e, sev, id]))
                    .collect(),
            )
        })
        .collect()
}

/// Per-row inline diagnostic virtual text `[text, severity, style_id | null] | null`.
fn diag_virt_to_json(rows: &[Option<DiagVirt>]) -> Vec<Json> {
    rows.iter()
        .map(|row| match row {
            Some((text, sev, id)) => json!([text, sev, id]),
            None => Json::Null,
        })
        .collect()
}

/// Per-row gutter diagnostic sign `[glyph, severity, style_id | null] | null`.
fn diag_signs_to_json(rows: &[Option<DiagSign>]) -> Vec<Json> {
    rows.iter()
        .map(|row| match row {
            Some((glyph, sev, id)) => json!([glyph, sev, id]),
            None => Json::Null,
        })
        .collect()
}

/// Per-row inlay hints `[col, text, style_id | null]`.
fn inlay_hints_to_json(rows: &[Vec<InlayHint>]) -> Vec<Json> {
    rows.iter()
        .map(|row| {
            Json::Array(
                row.iter()
                    .map(|(col, text, id)| json!([col, text, id]))
                    .collect(),
            )
        })
        .collect()
}

/// The float border as a lowercase name the renderer maps to box-drawing glyphs; `None`
/// for a borderless float or a tiled window.
fn border_name(border: Option<nxvim_view::Border>) -> Option<&'static str> {
    use nxvim_view::Border;
    border.map(|b| match b {
        Border::Single => "single",
        Border::Rounded => "rounded",
        Border::Double => "double",
        Border::Solid => "solid",
    })
}
