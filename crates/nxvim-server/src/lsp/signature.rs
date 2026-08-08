//! Signature-help **layout**: turning one server's [`SignatureInfo`] into the
//! lines the `[Signature]` doc float shows.
//!
//! A call signature is the one LSP payload that routinely outgrows a popup —
//! `def connect(host: str, port: int = 5432, timeout: float = 30.0, ssl: bool =
//! False, retries: int = 3) -> Connection` is a single unreadable run, and the
//! float caps at 80 columns, so the tail wraps into an unaligned second row. So a
//! signature with more than one parameter is laid out **one parameter per line**,
//! and the parameter the cursor sits in is marked.
//!
//! The split is *structural*, never punctuational: the ranges come from the
//! protocol's own `ParameterInformation.label` ([`SignatureInfo::parameters`]),
//! so `f(items: dict[str, int], key: tuple[A, B])` breaks into two parameters and
//! not four. When a server gives parameters that cannot be located in its own
//! label (or gives none at all), there is nothing to split on and the signature
//! renders as a single line — with the active parameter named in brackets, since
//! nothing else says which of them you are on.
//!
//! The marker is *not* in the text: core paints it as an overlay extmark over the
//! indent (see [`Editor::open_signature_float`](nxvim_core::Editor::open_signature_float)),
//! so the lines stay valid code for the tree-sitter pass that colors them. This
//! module only guarantees the indent is there to draw on.

use nxvim_core::{SIGNATURE_MARKER_COL, SIGNATURE_PARAM_INDENT};
use nxvim_lsp::SignatureInfo;

/// One server's signature, rendered.
pub(crate) struct SignatureLayout {
    /// The display lines, in order.
    pub(crate) lines: Vec<String>,
    /// Index into [`lines`](Self::lines) of the row holding the active parameter,
    /// i.e. the row core overlays the marker on. `None` when the server named no
    /// active parameter, or when the signature stayed on one line (nothing to
    /// point at).
    pub(crate) active_row: Option<usize>,
    /// Index into [`lines`](Self::lines) of the section-header row naming the server
    /// ([`with_server_name`]), for the float to draw its rule on. `None` for a lone
    /// contributor — naming the only server there is would be noise.
    pub(crate) header_row: Option<usize>,
}

/// Lay out one signature: one parameter per line when the server gave locatable
/// spans for two or more of them, else the label as a single line.
///
/// The vertical form keeps the label's own text verbatim — the leader up to the
/// first parameter (`def connect(`), each parameter, and the trailer after the
/// last (`) -> Connection`) are slices of the label, so nothing is invented:
///
/// ```text
/// def connect(
///     host: str,
///     port: int = 5432,      <- marked when active
/// ) -> Connection
/// ```
///
/// The separator each parameter line ends with is the label's *own* text between
/// that parameter and the next, trimmed (`", "` → `","`) — a server that spells
/// its separator differently keeps it. The last parameter reuses the first
/// separator, so the list ends on a trailing comma rather than looking truncated.
pub(crate) fn layout_signature(info: &SignatureInfo) -> SignatureLayout {
    let Some(spans) = info.layout_spans() else {
        return single_line(info);
    };
    let label = &info.label;
    let mut lines = Vec::with_capacity(spans.len() + 2);

    // The leader: everything before the first parameter (`def connect(`). Empty
    // for a label that *is* just its parameter list, in which case there is no
    // leader row and the parameters start at row 0.
    let leader = label[..spans[0].0].trim_end();
    if !leader.is_empty() {
        lines.push(leader.to_string());
    }
    let first_row = lines.len();

    // The separator between parameter 0 and 1 — the label's own (`", "` → `","`).
    // It exists because `layout_spans` guarantees at least two parameters, and it
    // is what the final parameter borrows so the list closes on a trailing one.
    let first_sep = label[spans[0].1..spans[1].0].trim().to_string();
    for (i, &(start, end)) in spans.iter().enumerate() {
        let sep = match spans.get(i + 1) {
            Some(&(next_start, _)) => label[end..next_start].trim(),
            None => &first_sep,
        };
        lines.push(format!(
            "{SIGNATURE_PARAM_INDENT}{}{sep}",
            &label[start..end]
        ));
    }

    // The trailer: everything after the last parameter (`) -> Connection`).
    let trailer = label[spans[spans.len() - 1].1..].trim_start();
    if !trailer.is_empty() {
        lines.push(trailer.to_string());
    }
    SignatureLayout {
        lines,
        active_row: info.active.map(|i| first_row + i),
        header_row: None,
    }
}

/// The single-line rendering: the label as the server spelled it. Used for a
/// one-parameter (or parameterless) signature — where a vertical split would cost
/// three rows to say what one already says — and as the fallback for a server whose
/// parameters cannot be located in its own label.
///
/// The active parameter is appended in brackets only when naming it *says* something.
/// With a **single** parameter it does not: `fn only(a: i32)    [a: i32]` echoes the
/// only thing the line already shows, and pushes the popup wider than the signature
/// it exists to show — there is one candidate, so showing the signature is showing
/// the active parameter. The bracket stays for the unlocatable fallback, where a
/// server's several parameters are all still on one line and naming one is the only
/// way to point at it.
fn single_line(info: &SignatureInfo) -> SignatureLayout {
    let line = match &info.active_text {
        Some(param) if !param.is_empty() && !info.has_sole_parameter() => {
            format!("{}    [{param}]", info.label)
        }
        _ => info.label.clone(),
    };
    SignatureLayout {
        lines: vec![line],
        active_row: None,
        header_row: None,
    }
}

/// Head a laid-out signature with the name of the server that produced it, for a
/// merged round where two servers answer and an unlabelled pair of signatures says
/// nothing about which language tool claims what.
///
/// The heading is a **section header row** — `─ pyright ─────`, the same labelled
/// rule the hover float parts its per-server sections with (the fill and the styling
/// are painted by [`Editor::open_signature_float`](nxvim_core::Editor::open_signature_float),
/// which is why [`header_row`](SignatureLayout::header_row) reports where it landed).
/// A row of its own even for a one-line signature: a `pyright: def f(x)` prefix reads
/// as part of the code it heads, and the two forms would then head differently.
pub(crate) fn with_server_name(layout: SignatureLayout, name: &str) -> SignatureLayout {
    let mut lines = Vec::with_capacity(layout.lines.len() + 1);
    lines.push(nxvim_core::markdown::section_header_line(name));
    lines.extend(layout.lines);
    SignatureLayout {
        lines,
        active_row: layout.active_row.map(|row| row + 1),
        header_row: Some(0),
    }
}

// The marker is drawn *inside* the indent, so the indent has to be wide enough to
// hold it and still leave a space before the parameter text.
const _: () = assert!(SIGNATURE_MARKER_COL + 2 <= SIGNATURE_PARAM_INDENT.len());
