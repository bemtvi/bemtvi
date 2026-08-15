//! The web client's column math, as wasm.
//!
//! Every client has to split a line into grapheme clusters and measure each one in
//! screen cells, because that is the grid the server's `bemtvi_core::unicode::virtcol`
//! puts every highlight span, selection and cursor column on. The TUI and the GUI link
//! `unicode-segmentation` + `unicode-width` and get it for free. The web client's DOM
//! renderer runs on the main thread while the wasm edit-host runs in a Web Worker, so
//! it cannot call into the editor — it used to mirror `unicode-width` in JS, from
//! generated tables plus a hand-written rule for the emoji ligatures. A mirror can
//! drift; this crate removes it by shipping the real thing as a ~64 KB module the page
//! instantiates once and calls synchronously.
//!
//! # Measuring like `virtcol`, not like `UnicodeWidthStr`
//!
//! [`segment`] measures **per cluster** and the caller sums, which is exactly what
//! `virtcol` does. That is deliberately NOT `UnicodeWidthStr::width` of the whole line:
//! that additionally ligatures across cluster boundaries (Arabic lam-alef is 1 cell to
//! it, 2 columns to `virtcol`), and a cell grid has to follow the editor's model.
//!
//! # ABI
//!
//! Two `Vec`s live in module memory. The caller [`reserve`]s room for the line's UTF-8,
//! writes it into the returned pointer, and calls [`segment`]; the per-cluster results
//! land in the output buffer at [`out_ptr`] as `(byte_len, cells)` `u32` pairs.
//!
//! `reserve` can grow wasm memory, which **detaches every existing `ArrayBuffer` view**
//! on the JS side — re-read `instance.exports.memory.buffer` after calling it, and
//! never hold a view across the call.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// The line's UTF-8 bytes, written by the caller.
static mut INPUT: Vec<u8> = Vec::new();
/// `(byte_len, cells)` pairs, one per grapheme cluster, written by [`segment`].
static mut OUTPUT: Vec<u32> = Vec::new();

// `&mut *addr_of_mut!(..)` rather than `&mut STATIC`: the latter is the `static_mut_refs`
// lint. Sound here because wasm32-unknown-unknown is single-threaded and every entry
// point below runs to completion before the caller can re-enter.
fn input() -> &'static mut Vec<u8> {
    unsafe { &mut *core::ptr::addr_of_mut!(INPUT) }
}
fn output() -> &'static mut Vec<u32> {
    unsafe { &mut *core::ptr::addr_of_mut!(OUTPUT) }
}

/// Make room for `len` input bytes and return the pointer to write them at.
///
/// May grow wasm memory — see the ABI note on detached views.
#[no_mangle]
pub extern "C" fn reserve(len: usize) -> *mut u8 {
    let buf = input();
    buf.clear();
    buf.resize(len, 0);
    buf.as_mut_ptr()
}

/// Split the `len` bytes at the input pointer into grapheme clusters, writing a
/// `(byte_len, cells)` `u32` pair per cluster to [`out_ptr`]; returns the cluster count.
///
/// The byte lengths sum to `len`, so the caller can slice its own copy of the text
/// without a second pass. Invalid UTF-8 is replaced rather than rejected: the bytes come
/// from a JS `TextEncoder`, so it cannot happen, and a panic here would take the page
/// down for a rendering detail.
#[no_mangle]
pub extern "C" fn segment(len: usize) -> usize {
    let bytes = &input()[..len.min(input().len())];
    let text = String::from_utf8_lossy(bytes);
    let out = output();
    out.clear();
    for g in text.graphemes(true) {
        out.push(g.len() as u32);
        out.push(UnicodeWidthStr::width(g) as u32);
    }
    out.len() / 2
}

/// Pointer to the `(byte_len, cells)` pairs [`segment`] wrote. Valid until the next
/// `segment` / `reserve` call.
#[no_mangle]
pub extern "C" fn out_ptr() -> *const u32 {
    output().as_ptr()
}
