//! The `'statusline'` `%`-format engine — pure, synchronous, and Lua-free, so it
//! lives in core where every front end shares it (and the `tabline` reuses it
//! verbatim).
//!
//! The `%`-format mini-language splits cleanly along nxvim's purity boundary, so
//! rendering is three pure passes plus an injected escape hatch for expressions:
//!
//! 1. [`parse`] turns a format string into a flat `Vec<Item>` — literals,
//!    built-in [`Field`]s, highlight switches, the `%=`/`%<` structural markers,
//!    and the `%{}`/`%!`/`%{%…%}` expression items.
//! 2. [`expand`] walks the items against a [`StatuslineCtx`] (the pre-computed
//!    editor facts a field needs), calling an injected `eval` callback for the
//!    expression items, and produces a flat sequence of [`Piece`]s: resolved text
//!    carrying its active highlight group, with the align/truncate markers kept
//!    in place for the next pass.
//! 3. [`layout`] resolves `%=` alignment and `%<` truncation against the final
//!    width into the [`StatusSegment`]s a client paints.
//!
//! Expression evaluation (`%{}` etc.) is the one thing core can't do — it needs
//! Lua — so [`expand`] takes a `&mut dyn FnMut(ExprKind, &str) -> String`
//! callback. The **server** supplies a closure wrapping its synchronous Lua eval
//! (and enforces the `v:lua`-only rule); core stays pure.
//!
//! The semantics are ported field-for-field from neovim's `build_stl_str_hl`
//! (`vendor/neovim/src/nvim/statusline.c`) and its `get_rel_pos` /
//! `calc_percentage` helpers — not the C itself (which is wired into a `win_T`
//! and the Vimscript evaluator, neither of which fits here), only the small
//! self-contained integer algorithms. The unit tests pin each one to
//! `nvim_eval_statusline` ground truth.

use unicode_width::UnicodeWidthChar;

/// A single built-in `%`-item that core can compute itself from a
/// [`StatuslineCtx`] (everything that is *not* an embedded expression). Names
/// mirror neovim's `:help 'statusline'` item letters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// `%f` — path to the file, as typed / relative to the cwd.
    FileRel,
    /// `%F` — full path to the file.
    FileFull,
    /// `%t` — file name (tail) only.
    FileTail,
    /// `%m` — modified flag: `[+]` if modified, `[-]` if 'modifiable' is off,
    /// else empty.
    Modified,
    /// `%M` — modified flag without brackets: `+`, `-`, or empty.
    ModifiedComma,
    /// `%r` — readonly flag: `[RO]` or empty.
    ReadOnly,
    /// `%h` — help-buffer flag: `[Help]` or empty.
    Help,
    /// `%y` — filetype in brackets: `[rust]`, or empty when unset.
    FileType,
    /// `%n` — buffer number.
    BufNr,
    /// `%l` — cursor line number (1-based).
    Line,
    /// `%L` — number of lines in the buffer.
    LineCount,
    /// `%c` — cursor column (1-based byte column).
    Col,
    /// `%v` — cursor virtual column (1-based).
    VirtCol,
    /// `%p` — percentage through the file in lines, as in the ruler.
    Percentage,
    /// `%P` — percentage of the displayed window: `Top`/`Bot`/`All`/`nn%`.
    AltPercentage,
}

/// How an embedded expression item is interpreted once evaluated. (Core never
/// evaluates; it only records the kind so the injected `eval` callback and
/// [`expand`] know what to do with the result.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprKind {
    /// `%{expr}` — the result is plain literal text.
    Eval,
    /// `%{%expr%}` — the result is itself a format string, re-parsed as items.
    EvalItems,
    /// `%!expr` — the result *is* the whole statusline, re-parsed as items.
    Whole,
}

/// What a click region ([`ClickRegion`]) does when its cells are clicked. A region
/// is opened by an `%@…@` (a Lua handler) or an `%nT` (a tabline tab-select) and
/// closed by the next [`Item::ClickEnd`] (`%X`/`%T`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClickAction {
    /// `%@handler@` / `%N@handler@` — call the Lua handler (a `v:lua.…` reference,
    /// like `%{}`/`%!`) with `minwid` (the optional numeric prefix, `0` if omitted).
    Handler { handler: String, minwid: u32 },
    /// `%nT` (`n ≥ 1`) — switch to **tab page `n`** (1-based), the tabline's
    /// tab-select region. Backed by [`Editor::select_main_tab`](crate::Editor).
    Tab(usize),
}

/// One parsed element of a statusline format string. The output of [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Verbatim text (and the decoded `%%` → `%`).
    Literal(String),
    /// A built-in field, expanded from the [`StatuslineCtx`].
    Field(Field),
    /// A highlight switch: `%#Group#` / `%N*` → `Some(group)`, `%*` / `%0*` →
    /// `None` (reset to the base `StatusLine` highlight).
    HlSwitch(Option<String>),
    /// `%=` — a separation point; alignment fill is distributed here.
    Align,
    /// `%<` — the point at which the line is truncated when it is too wide.
    Truncate,
    /// The **start** of a clickable region: `%@handler@` / `%N@handler@` (a Lua
    /// handler) or `%nT` (`n ≥ 1`, a tabline tab-select) — see [`ClickAction`]. The
    /// region's text renders normally; only its column span is tracked (see
    /// [`ClickRegion`]). Terminated by the next [`Item::ClickEnd`].
    ClickStart { action: ClickAction },
    /// `%X` / `%nX` / `%T` / `%0T` — the **end** of a click region (neovim's region
    /// terminators: `%X` ends a handler / close-button region, a bare or `%0T` ends
    /// the tab labels). Closes the open [`Item::ClickStart`]; with none open it
    /// renders to nothing (carrying no text), so a label-only `%T…%X` format is
    /// unaffected.
    ClickEnd,
    /// `%{…}` / `%!…` / `%{%…%}` — an expression evaluated by the injected
    /// callback. `raw` is the expression text between the delimiters.
    Expr { kind: ExprKind, raw: String },
}

/// The pre-computed editor facts the built-in [`Field`]s read. The server fills
/// this from the focused window + its buffer each redraw; core never reaches
/// into editor state itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslineCtx {
    /// `%f` — the file path as displayed (relative to cwd / as typed).
    pub file_rel: String,
    /// `%F` — the full file path.
    pub file_full: String,
    /// `%t` — the file-name tail.
    pub file_tail: String,
    /// Whether the buffer has unsaved changes (`%m` / `%M`).
    pub modified: bool,
    /// Whether the buffer is 'modifiable' (`%m` / `%M` show `[-]`/`-` when off).
    pub modifiable: bool,
    /// Whether the buffer is read-only (`%r`).
    pub readonly: bool,
    /// Whether this is a help buffer (`%h`).
    pub help: bool,
    /// The filetype, bare (e.g. `rust`); `%y` wraps it in brackets. Empty ⇒ `%y`
    /// expands to nothing.
    pub filetype: String,
    /// The buffer's `'fileencoding'` (e.g. `utf-8`, `latin1`, `utf-16le`) — the
    /// charset its bytes are in on disk. nxvim's built-in default status line shows
    /// it (neovim has no `%`-letter for it; it's conventionally `%{&fenc}`).
    pub fileencoding: String,
    /// Whether the buffer writes a byte-order mark (`'bomb'`); shown as a `[bom]`
    /// suffix on the encoding in the default status line.
    pub bomb: bool,
    /// The buffer number (`%n`).
    pub bufnr: usize,
    /// 1-based cursor line (`%l`).
    pub line: usize,
    /// Number of lines in the buffer (`%L`).
    pub line_count: usize,
    /// 1-based cursor byte column (`%c`).
    pub col: usize,
    /// 1-based cursor virtual column (`%v`).
    pub virtcol: usize,
    /// 1-based number of the first visible line in the window (`%P`).
    pub top_line: usize,
    /// Number of visible text rows in the window (`%P`).
    pub text_height: usize,
    /// Diagnostic counts for the window's buffer, by severity
    /// `[error, warn, info, hint]`. Read by the `diagnostics` built-in
    /// [segment](compose_segments); zero on builds without an LSP (the browser
    /// edit-host) and on the `%`-format path (which has no `%`-letter for them).
    pub diag_counts: [usize; 4],
}

/// The output of [`expand`]: resolved text carrying its active highlight group,
/// with the structural markers preserved so [`layout`] can place fill and the
/// truncation cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Resolved text and the highlight group active over it (`None` ⇒ the base
    /// `StatusLine`).
    Text { text: String, group: Option<String> },
    /// A `%=` separation point.
    Align,
    /// A `%<` truncation point.
    Truncate,
    /// The start of a click region (`%@handler@` or `%nT`): the text that follows,
    /// up to the matching [`Piece::ClickEnd`], is clickable with [`ClickAction`].
    /// [`layout_with_clicks`] tracks its column span.
    ClickStart { action: ClickAction },
    /// The end of a click region (`%X` / `%T`).
    ClickEnd,
}

/// One painted run of the final status line: a string and the highlight group it
/// is drawn in (`None` ⇒ the base `StatusLine`). The output of [`layout`] and
/// what the server projects / the client paints.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StatusSegment {
    pub text: String,
    pub group: Option<String>,
    /// A click handler for an `nx.statusline` segment cell (a `v:lua.…` reference),
    /// or `None` for a non-clickable cell. Set only on custom-segment cells (built-in
    /// segments and the `%`-format path leave it `None` — the latter tracks clicks as
    /// [`ClickRegion`]s instead). [`compose_segments_with_clicks`] turns a `Some` cell
    /// into a [`ClickRegion`] by wrapping it in a [`Piece::ClickStart`]/`ClickEnd`.
    pub on_click: Option<String>,
}

/// Parse a `'statusline'` format string into a flat list of [`Item`]s.
///
/// Recognised `%`-items: the [`Field`] letters, `%%` (a literal `%`), `%=`
/// (align), `%<` (truncate), highlight switches `%#Group#` / `%*` / `%0*` /
/// `%N*`, the tabline tab-select markers `%T` / `%nT`, the click-region items
/// `%@handler@` / `%N@handler@` … `%X` / `%nX`, and the expression forms
/// `%{…}`, `%{%…%}`, `%!…`. Any other `%`-sequence is an error
/// (no silent passthrough — an unknown item would otherwise render as misleading
/// text), returned as a human-readable message naming the offending item.
pub fn parse(fmt: &str) -> Result<Vec<Item>, String> {
    let bytes = fmt.as_bytes();
    let mut items = Vec::new();
    // Accumulated literal text, flushed whenever a `%`-item interrupts it.
    let mut lit = String::new();
    let mut i = 0;

    macro_rules! flush_lit {
        () => {
            if !lit.is_empty() {
                items.push(Item::Literal(std::mem::take(&mut lit)));
            }
        };
    }

    while i < bytes.len() {
        let c = bytes[i];
        if c != b'%' {
            // Copy one full UTF-8 char into the literal buffer.
            let ch_len = utf8_len(c);
            lit.push_str(&fmt[i..i + ch_len]);
            i += ch_len;
            continue;
        }

        // `c == '%'` — decode the item that follows.
        let next = bytes.get(i + 1).copied();
        match next {
            None => return Err("statusline: trailing '%' with no item".to_string()),
            Some(b'%') => {
                lit.push('%');
                i += 2;
            }
            Some(b'=') => {
                flush_lit!();
                items.push(Item::Align);
                i += 2;
            }
            Some(b'<') => {
                flush_lit!();
                items.push(Item::Truncate);
                i += 2;
            }
            Some(b'#') => {
                // `%#Group#` — group name runs to the closing `#`.
                flush_lit!();
                let rest = &fmt[i + 2..];
                let end = rest
                    .find('#')
                    .ok_or_else(|| "statusline: unterminated %#Group# highlight".to_string())?;
                items.push(Item::HlSwitch(Some(rest[..end].to_string())));
                i += 2 + end + 1;
            }
            Some(b'*') => {
                // Bare `%*` — reset to the base highlight.
                flush_lit!();
                items.push(Item::HlSwitch(None));
                i += 2;
            }
            Some(b'T') | Some(b'X') => {
                // Bare `%T` / `%X` — the end of a click region (neovim's tab-label
                // / close-button terminators). No text.
                flush_lit!();
                items.push(Item::ClickEnd);
                i += 2;
            }
            Some(b'@') => {
                // `%@handler@` — the start of a Lua-handler click region; the handler
                // name runs to the next `@`. No numeric prefix ⇒ `minwid` 0.
                flush_lit!();
                let rest = &fmt[i + 2..];
                let end = rest.find('@').ok_or_else(|| {
                    "statusline: unterminated %@handler@ click region".to_string()
                })?;
                items.push(Item::ClickStart {
                    action: ClickAction::Handler {
                        handler: rest[..end].to_string(),
                        minwid: 0,
                    },
                });
                i += 2 + end + 1;
            }
            Some(b'!') => {
                // `%!expr` — the whole statusline is the eval result. The
                // expression runs to the end of the format string.
                flush_lit!();
                let raw = fmt[i + 2..].to_string();
                items.push(Item::Expr {
                    kind: ExprKind::Whole,
                    raw,
                });
                break;
            }
            Some(b'{') => {
                flush_lit!();
                let (item, consumed) = parse_brace_expr(&fmt[i..])?;
                items.push(item);
                i += consumed;
            }
            Some(d) if d.is_ascii_digit() => {
                // A `%`-item carrying a leading number: `%N*` (User highlight), the
                // numbered tabline regions `%nT` / `%nX`, or a `%N@handler@` click
                // region with `minwid` N. Read the whole digit run, then dispatch on
                // the letter that closes it.
                let mut j = i + 1;
                while bytes.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
                match bytes.get(j).copied() {
                    // `%N*` (highlight) — `%0*` resets, `%1*`..`%9*` select
                    // User{N}. Single digit only, as in neovim.
                    Some(b'*') if j == i + 2 => {
                        flush_lit!();
                        let group = if d == b'0' {
                            None
                        } else {
                            Some(format!("User{}", (d - b'0')))
                        };
                        items.push(Item::HlSwitch(group));
                        i = j + 1;
                    }
                    // `%nT` — a numbered tabline tab-select region: `n ≥ 1` opens a
                    // click region for tab page `n`; `%0T` is the label-end marker.
                    Some(b'T') => {
                        flush_lit!();
                        // The digit run is in-range ASCII; saturate an absurd one.
                        let n = fmt[i + 1..j].parse::<usize>().unwrap_or(usize::MAX);
                        items.push(if n == 0 {
                            Item::ClickEnd
                        } else {
                            Item::ClickStart {
                                action: ClickAction::Tab(n),
                            }
                        });
                        i = j + 1;
                    }
                    // `%nX` — a numbered region terminator / close button.
                    Some(b'X') => {
                        flush_lit!();
                        items.push(Item::ClickEnd);
                        i = j + 1;
                    }
                    // `%N@handler@` — a Lua-handler click region carrying `minwid` N.
                    Some(b'@') => {
                        flush_lit!();
                        let minwid = fmt[i + 1..j].parse::<u32>().unwrap_or(u32::MAX);
                        let rest = &fmt[j + 1..];
                        let end = rest.find('@').ok_or_else(|| {
                            "statusline: unterminated %@handler@ click region".to_string()
                        })?;
                        items.push(Item::ClickStart {
                            action: ClickAction::Handler {
                                handler: rest[..end].to_string(),
                                minwid,
                            },
                        });
                        i = j + 1 + end + 1;
                    }
                    // A digit prefix on any other item is a width field, which v1
                    // does not support.
                    _ => {
                        return Err(format!(
                            "statusline: width-prefixed items (%{}…) are not supported yet",
                            d as char
                        ));
                    }
                }
            }
            Some(f) => {
                let field = field_from_byte(f)
                    .ok_or_else(|| format!("statusline: unknown format item %{}", f as char))?;
                flush_lit!();
                items.push(Item::Field(field));
                i += 2;
            }
        }
    }
    flush_lit!();
    Ok(items)
}

/// Parse a `%{…}` / `%{%…%}` item starting at `s[0] == '%'`, `s[1] == '{'`.
/// Returns the item and the number of bytes consumed (including the `%{` and the
/// closing `}`). `%{%…%}` (the inner-format form) is detected by a leading `%`
/// inside the braces and a `%}` close.
fn parse_brace_expr(s: &str) -> Result<(Item, usize), String> {
    // s starts with "%{".
    let inner_start = 2;
    let is_eval_items = s[inner_start..].starts_with('%');
    if is_eval_items {
        // `%{%expr%}` — find the closing `%}`.
        let body = &s[inner_start + 1..];
        let end = body
            .find("%}")
            .ok_or_else(|| "statusline: unterminated %{%…%} expression".to_string())?;
        let raw = body[..end].to_string();
        let consumed = inner_start + 1 + end + 2;
        Ok((
            Item::Expr {
                kind: ExprKind::EvalItems,
                raw,
            },
            consumed,
        ))
    } else {
        // `%{expr}` — find the closing `}`.
        let body = &s[inner_start..];
        let end = body
            .find('}')
            .ok_or_else(|| "statusline: unterminated %{…} expression".to_string())?;
        let raw = body[..end].to_string();
        let consumed = inner_start + end + 1;
        Ok((
            Item::Expr {
                kind: ExprKind::Eval,
                raw,
            },
            consumed,
        ))
    }
}

/// Map a built-in field letter to its [`Field`], or `None` for an unknown letter.
fn field_from_byte(b: u8) -> Option<Field> {
    Some(match b {
        b'f' => Field::FileRel,
        b'F' => Field::FileFull,
        b't' => Field::FileTail,
        b'm' => Field::Modified,
        b'M' => Field::ModifiedComma,
        b'r' => Field::ReadOnly,
        b'h' => Field::Help,
        b'y' => Field::FileType,
        b'n' => Field::BufNr,
        b'l' => Field::Line,
        b'L' => Field::LineCount,
        b'c' => Field::Col,
        b'v' => Field::VirtCol,
        b'p' => Field::Percentage,
        b'P' => Field::AltPercentage,
        _ => return None,
    })
}

/// Length in bytes of the UTF-8 sequence whose leading byte is `b`.
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Expand parsed [`Item`]s against `ctx` into [`Piece`]s, calling `eval` for each
/// expression item. The active highlight group threads through the walk: an
/// `HlSwitch` changes it, every text piece thereafter carries it.
///
/// Expression results are folded in per [`ExprKind`]: `Eval` text is a literal
/// piece; `EvalItems` / `Whole` results are re-parsed and expanded inline (so a
/// `%{%…%}` may itself emit highlight switches and fields). A `Whole` item (a
/// `%!…` statusline) replaces everything — its result is the entire line — so it
/// only makes sense as the sole item, which is how [`parse`] produces it.
pub fn expand(
    items: &[Item],
    ctx: &StatuslineCtx,
    eval: &mut dyn FnMut(ExprKind, &str) -> String,
) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut group: Option<String> = None;
    expand_into(items, ctx, eval, &mut group, &mut out);
    out
}

/// The recursive worker behind [`expand`]: appends pieces to `out`, threading the
/// active highlight `group` so re-parsed sub-formats continue under the group in
/// force where they were spliced in.
fn expand_into(
    items: &[Item],
    ctx: &StatuslineCtx,
    eval: &mut dyn FnMut(ExprKind, &str) -> String,
    group: &mut Option<String>,
    out: &mut Vec<Piece>,
) {
    for item in items {
        match item {
            Item::Literal(text) => push_text(out, text.clone(), group.clone()),
            Item::Field(f) => push_text(out, expand_field(*f, ctx), group.clone()),
            Item::HlSwitch(g) => *group = g.clone(),
            Item::Align => out.push(Piece::Align),
            Item::Truncate => out.push(Piece::Truncate),
            Item::ClickStart { action } => out.push(Piece::ClickStart {
                action: action.clone(),
            }),
            Item::ClickEnd => out.push(Piece::ClickEnd),
            Item::Expr { kind, raw } => {
                let result = eval(*kind, raw);
                match kind {
                    ExprKind::Eval => push_text(out, result, group.clone()),
                    ExprKind::EvalItems | ExprKind::Whole => {
                        // The result is itself a format string: re-parse and
                        // expand it inline. A parse error in the (Lua-produced)
                        // sub-format is surfaced as literal text rather than
                        // dropped, so the failure is visible on the status line.
                        match parse(&result) {
                            Ok(sub) => expand_into(&sub, ctx, eval, group, out),
                            Err(e) => push_text(out, e, group.clone()),
                        }
                    }
                }
            }
        }
    }
}

/// Append `text` to `out`, coalescing with the previous piece when it is text in
/// the same highlight group (keeps the piece list tidy and matches how a client
/// would paint contiguous same-group runs). Empty text is dropped.
fn push_text(out: &mut Vec<Piece>, text: String, group: Option<String>) {
    if text.is_empty() {
        return;
    }
    if let Some(Piece::Text {
        text: prev,
        group: pg,
    }) = out.last_mut()
    {
        if *pg == group {
            prev.push_str(&text);
            return;
        }
    }
    out.push(Piece::Text { text, group });
}

/// Expand a single built-in [`Field`] to its text, per neovim's item semantics.
fn expand_field(field: Field, ctx: &StatuslineCtx) -> String {
    match field {
        Field::FileRel => ctx.file_rel.clone(),
        Field::FileFull => ctx.file_full.clone(),
        Field::FileTail => ctx.file_tail.clone(),
        Field::Modified => modified_flag(ctx, "[+]", "[-]"),
        Field::ModifiedComma => modified_flag(ctx, "+", "-"),
        Field::ReadOnly => if ctx.readonly { "[RO]" } else { "" }.to_string(),
        Field::Help => if ctx.help { "[Help]" } else { "" }.to_string(),
        Field::FileType => {
            if ctx.filetype.is_empty() {
                String::new()
            } else {
                format!("[{}]", ctx.filetype)
            }
        }
        Field::BufNr => ctx.bufnr.to_string(),
        Field::Line => ctx.line.to_string(),
        Field::LineCount => ctx.line_count.to_string(),
        Field::Col => ctx.col.to_string(),
        Field::VirtCol => ctx.virtcol.to_string(),
        Field::Percentage => percentage(ctx.line, ctx.line_count).to_string(),
        Field::AltPercentage => alt_percentage(ctx),
    }
}

/// The `%m` / `%M` modified flag: `modified` text when there are unsaved changes,
/// the `nomodifiable` text when 'modifiable' is off, else empty. (neovim shows the
/// modified marker in preference to the nomodifiable one.)
fn modified_flag(ctx: &StatuslineCtx, modified: &str, nomod: &str) -> String {
    if ctx.modified {
        modified.to_string()
    } else if !ctx.modifiable {
        nomod.to_string()
    } else {
        String::new()
    }
}

/// `%p` — `part * 100 / whole`, truncated (neovim's `calc_percentage`). Zero when
/// the buffer is empty, to avoid a divide-by-zero.
fn percentage(part: usize, whole: usize) -> usize {
    (part * 100).checked_div(whole).unwrap_or(0)
}

/// `%P` — neovim's `get_rel_pos`: `All` / `Top` / `Bot`, or a 3-wide
/// right-justified `nn%`, from how much of the buffer sits above / below the
/// window.
fn alt_percentage(ctx: &StatuslineCtx) -> String {
    let above = ctx.top_line.saturating_sub(1) as isize;
    let bot_line = ctx.top_line + ctx.text_height; // first line below the window
    let below = ctx.line_count as isize - bot_line as isize + 1;
    if below <= 0 {
        return if above == 0 { "All" } else { "Bot" }.to_string();
    }
    if above <= 0 {
        return "Top".to_string();
    }
    let perc = percentage(above as usize, (above + below) as usize);
    format!("{:>3}", format!("{perc}%"))
}

/// Resolve [`Piece`]s into the final [`StatusSegment`]s for a status line of
/// `width` cells: apply `%<` truncation when the text overflows, otherwise
/// distribute `%=` alignment fill. A `width` of `0` means "unconstrained" — no
/// truncation, no fill (the markers collapse and the raw text is returned).
///
/// Faithful to neovim's post-processing in `build_stl_str_hl`: over-budget lines
/// truncate at the `%<` point (or the first item, or the start), keeping the tail
/// after a `<`; under-budget lines with `%=` markers split the slack between the
/// markers, the last marker taking the remainder.
pub fn layout(pieces: &[Piece], width: usize) -> Vec<StatusSegment> {
    coalesce(resolve_chars(flatten(pieces), width))
}

/// A clickable region of the laid-out status line: the half-open display-column
/// span `[start_col, end_col)` (0-based, in the line the client paints) and what a
/// click there does ([`ClickAction`] — a Lua handler or a tab-select). The output
/// of [`layout_with_clicks`]; the server resolves a status/tabline click's column
/// to one of these. Spans are tracked through truncation/fill (they ride each
/// cell), so a truncated region reports its surviving span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickRegion {
    pub start_col: usize,
    pub end_col: usize,
    pub action: ClickAction,
}

/// [`layout`], also returning the [`ClickRegion`]s (`%@…%X` / `%nT`) with their
/// final display-column spans. Used by the server's `%`-format render so a later
/// status/tabline click can be resolved to its action. (The plain [`layout`] is
/// kept for the callers — and the parser unit tests — that don't need click regions.)
pub fn layout_with_clicks(
    pieces: &[Piece],
    width: usize,
) -> (Vec<StatusSegment>, Vec<ClickRegion>) {
    let flat = flatten(pieces);
    let regions = flat.regions.clone();
    let chars = resolve_chars(flat, width);
    let clicks = collect_clicks(&chars, &regions);
    (coalesce(chars), clicks)
}

/// The width-dependent pass shared by [`layout`] / [`layout_with_clicks`]: apply
/// `%<` truncation when the text overflows, otherwise distribute `%=` fill. A
/// `width` of `0` means "unconstrained" — the markers collapse and the raw cells
/// are returned.
fn resolve_chars(flat: Flat, width: usize) -> Vec<Cell> {
    let total: usize = flat.chars.iter().map(|c| c.width).sum();
    if width == 0 {
        flat.chars
    } else if total > width {
        truncate(
            flat.chars,
            &flat.align_positions,
            flat.trunc_position,
            flat.first_item,
            width,
        )
    } else if total < width && !flat.align_positions.is_empty() {
        distribute_fill(flat.chars, &flat.align_positions, width - total)
    } else {
        flat.chars
    }
}

/// Walk the laid-out cells and group maximal runs sharing a click-region index
/// into [`ClickRegion`]s, summing display widths into column spans. Cells outside
/// any region (`click == None`) — the inter-region text, `%=` fill, the `<`/`>`
/// truncation markers — break a run. A region whose every cell was truncated away
/// simply yields no span.
fn collect_clicks(chars: &[Cell], regions: &[ClickAction]) -> Vec<ClickRegion> {
    let mut out: Vec<ClickRegion> = Vec::new();
    let mut col = 0usize;
    // The open region: its index and the column it started at.
    let mut cur: Option<(usize, usize)> = None;
    let close = |out: &mut Vec<ClickRegion>, idx: usize, start: usize, end: usize| {
        if let Some(action) = regions.get(idx) {
            out.push(ClickRegion {
                start_col: start,
                end_col: end,
                action: action.clone(),
            });
        }
    };
    for c in chars {
        match (c.click, cur) {
            // Continue the open region.
            (Some(idx), Some((cur_idx, _))) if idx == cur_idx => {}
            // A different region begins immediately after another: close, reopen.
            (Some(idx), Some((cur_idx, start))) => {
                close(&mut out, cur_idx, start, col);
                let _ = cur_idx;
                cur = Some((idx, col));
            }
            // A region opens out of bare text.
            (Some(idx), None) => cur = Some((idx, col)),
            // Bare text closes any open region.
            (None, Some((cur_idx, start))) => {
                close(&mut out, cur_idx, start, col);
                cur = None;
            }
            (None, None) => {}
        }
        col += c.width;
    }
    if let Some((idx, start)) = cur {
        close(&mut out, idx, start, col);
    }
    out
}

/// A char of resolved status text: the character, its display width, the highlight
/// group active over it, and the click-region index it belongs to (`None` ⇒ not in
/// a `%@…%X` region). The region index keys into [`Flat::regions`].
#[derive(Clone)]
struct Cell {
    ch: char,
    width: usize,
    group: Option<String>,
    click: Option<usize>,
}

/// The char-level view of expanded pieces that [`layout`] operates on: every text
/// char with its group, plus the char-index positions of the structural markers.
struct Flat {
    chars: Vec<Cell>,
    /// Char index of each `%=` marker (in `chars` coordinates).
    align_positions: Vec<usize>,
    /// Char index of the `%<` marker, if any.
    trunc_position: Option<usize>,
    /// Char index of the first `%`-item of any kind — neovim's default
    /// truncation point when there is no `%<`. `None` ⇒ pure literal text.
    first_item: Option<usize>,
    /// Each click region's [`ClickAction`], indexed by the [`Cell::click`] that
    /// references it (assigned in encounter order as a `ClickStart` opens).
    regions: Vec<ClickAction>,
}

/// Lower [`Piece`]s to the char-level [`Flat`] form, recording marker positions
/// as char offsets into the emitted text and tagging each cell with the click
/// region in force (if any).
fn flatten(pieces: &[Piece]) -> Flat {
    let mut chars = Vec::new();
    let mut align_positions = Vec::new();
    let mut trunc_position = None;
    let mut first_item = None;
    let mut regions: Vec<ClickAction> = Vec::new();
    // The click region currently open (its index into `regions`), or `None`.
    let mut click: Option<usize> = None;
    for piece in pieces {
        match piece {
            Piece::Text { text, group } => {
                for ch in text.chars() {
                    chars.push(Cell {
                        ch,
                        width: UnicodeWidthChar::width(ch).unwrap_or(0),
                        group: group.clone(),
                        click,
                    });
                }
            }
            Piece::Align => {
                first_item.get_or_insert(chars.len());
                align_positions.push(chars.len());
            }
            Piece::Truncate => {
                first_item.get_or_insert(chars.len());
                trunc_position.get_or_insert(chars.len());
            }
            Piece::ClickStart { action } => {
                regions.push(action.clone());
                click = Some(regions.len() - 1);
            }
            Piece::ClickEnd => click = None,
        }
    }
    Flat {
        chars,
        align_positions,
        trunc_position,
        first_item,
        regions,
    }
}

/// Truncate an over-budget line to `width` cells. Ports the two branches of
/// neovim's truncation: if even the prefix before the cut point overflows, drop
/// the tail and mark the end with `>`; otherwise cut at the point, dropping cells
/// until the line fits and marking the cut with `<`.
fn truncate(
    chars: Vec<Cell>,
    align_positions: &[usize],
    trunc_position: Option<usize>,
    first_item: Option<usize>,
    width: usize,
) -> Vec<Cell> {
    // The cut point: the `%<` marker, else the first item, else the start. (The
    // align positions feed `first_item` already via `flatten`, but a `%<`
    // always wins.)
    let _ = align_positions;
    let cut = trunc_position.or(first_item).unwrap_or(0).min(chars.len());

    let prefix_w: usize = chars[..cut].iter().map(|c| c.width).sum();

    if prefix_w >= width {
        // The prefix alone overflows: keep as many leading cells as fit, then a
        // trailing `>`. Stop once adding the next cell would reach `width`
        // (leaving room for the marker).
        let mut kept = 0;
        let mut acc = 0;
        for c in &chars {
            if acc + c.width + 1 > width {
                break;
            }
            acc += c.width;
            kept += 1;
        }
        let group = chars
            .get(kept)
            .or_else(|| chars.last())
            .and_then(|c| c.group.clone());
        let mut out: Vec<Cell> = chars.into_iter().take(kept).collect();
        // The truncation marker is chrome, never part of a click region.
        out.push(Cell {
            ch: '>',
            width: 1,
            group,
            click: None,
        });
        out
    } else {
        // Cut at the point: drop cells starting there until the whole line fits,
        // then splice in a single `<`.
        let mut total: usize = prefix_w + chars[cut..].iter().map(|c| c.width).sum::<usize>();
        let mut drop_end = cut;
        while total >= width && drop_end < chars.len() {
            total -= chars[drop_end].width;
            drop_end += 1;
        }
        // The marker inherits the group of the cut region (the first dropped
        // cell, falling back to the cell before the cut).
        let group = chars
            .get(cut)
            .or_else(|| cut.checked_sub(1).and_then(|j| chars.get(j)))
            .and_then(|c| c.group.clone());
        let mut out: Vec<Cell> = chars[..cut].to_vec();
        // The truncation marker is chrome, never part of a click region.
        out.push(Cell {
            ch: '<',
            width: 1,
            group,
            click: None,
        });
        out.extend(chars[drop_end..].iter().map(|c| Cell {
            ch: c.ch,
            width: c.width,
            group: c.group.clone(),
            click: c.click,
        }));
        out
    }
}

/// Distribute `slack` fill cells across the `%=` markers of an under-budget line.
/// Each marker gets `slack / n` spaces; the last marker absorbs the remainder
/// (neovim's `standard_spaces` / `final_spaces` split). Fill inherits the group
/// active just before the marker.
fn distribute_fill(chars: Vec<Cell>, align_positions: &[usize], slack: usize) -> Vec<Cell> {
    let n = align_positions.len();
    let standard = slack / n;
    let final_spaces = slack - standard * (n - 1);

    let mut out = Vec::with_capacity(chars.len() + slack);
    let mut next_marker = 0;
    for (idx, c) in chars.iter().enumerate() {
        // Emit fill for any markers sitting at this char index (there can be more
        // than one when `%=%=` are adjacent).
        while next_marker < n && align_positions[next_marker] == idx {
            let count = if next_marker == n - 1 {
                final_spaces
            } else {
                standard
            };
            let group = idx
                .checked_sub(1)
                .and_then(|j| chars.get(j))
                .and_then(|c| c.group.clone());
            for _ in 0..count {
                out.push(Cell {
                    ch: ' ',
                    width: 1,
                    group: group.clone(),
                    // `%=` fill sits between regions, never inside one.
                    click: None,
                });
            }
            next_marker += 1;
        }
        out.push(c.clone());
    }
    // Markers sitting at the very end of the text.
    while next_marker < n {
        let count = if next_marker == n - 1 {
            final_spaces
        } else {
            standard
        };
        let group = chars.last().and_then(|c| c.group.clone());
        for _ in 0..count {
            out.push(Cell {
                ch: ' ',
                width: 1,
                group: group.clone(),
                // `%=` fill sits between regions, never inside one.
                click: None,
            });
        }
        next_marker += 1;
    }
    out
}

/// Merge a char sequence into [`StatusSegment`]s, joining adjacent cells that
/// share a highlight group into one run.
fn coalesce(chars: Vec<Cell>) -> Vec<StatusSegment> {
    let mut segments: Vec<StatusSegment> = Vec::new();
    for c in chars {
        if let Some(last) = segments.last_mut() {
            if last.group == c.group {
                last.text.push(c.ch);
                continue;
            }
        }
        segments.push(StatusSegment {
            text: c.ch.to_string(),
            group: c.group,
            on_click: None,
        });
    }
    segments
}

// ---------------------------------------------------------------------------
// Segment composition — the `nx.statusline` (lualine-shaped) surface.
//
// The `%`-format engine above is one way to drive the status line. The other is
// a declarative list of named *segments* (`nx.statusline.setup{ left=…, right=…
// }`). A segment resolves to a list of [`StatusSegment`] cells; built-in
// segments resolve here in core from the [`StatuslineCtx`] every frame (pure,
// cheap, per-window), while custom Lua segments are resolved by the server and
// passed in through the `custom` lookup (re-run only on their declared events,
// never per frame — see docs/specs/2026-06-11-native-plugin-api.md §2).
//
// Both paths converge on the same [`layout`] → [`StatusSegment`] output, so the
// server projection and every client paint are shared verbatim.
// ---------------------------------------------------------------------------

/// The declarative segment layout from `nx.statusline.setup{}`: ordered segment
/// *names* for the left and right halves (`%=` sits between them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentLayout {
    pub left: Vec<String>,
    pub right: Vec<String>,
}

/// Whether `name` is a built-in segment (resolved natively each frame from the
/// [`StatuslineCtx`]) rather than a custom Lua segment. The server uses this to
/// tell which names a `nx.statusline.setup{}` layout must re-render through Lua.
/// Keep in lockstep with the match in [`builtin_segment`].
pub fn is_builtin_segment(name: &str) -> bool {
    matches!(
        name,
        "mode"
            | "filename"
            | "filepath"
            | "filetype"
            | "encoding"
            | "location"
            | "modified"
            | "readonly"
            | "diagnostics"
    )
}

/// Resolve one **built-in** segment to its cells from the [`StatuslineCtx`].
///
/// Returns `Some(cells)` for a known built-in — including `Some(vec![])` when a
/// known segment has nothing to show right now (e.g. `filetype` with no
/// filetype set), so it simply contributes nothing — and `None` when `name` is
/// not a built-in at all (the caller then falls through to a custom segment, and
/// errors loudly if it is unknown everywhere).
pub fn builtin_segment(
    name: &str,
    ctx: &StatuslineCtx,
    mode_label: &str,
) -> Option<Vec<StatusSegment>> {
    let one = |text: String, group: Option<&str>| {
        if text.is_empty() {
            Vec::new()
        } else {
            vec![StatusSegment {
                text,
                group: group.map(str::to_string),
                on_click: None,
            }]
        }
    };
    Some(match name {
        "mode" => one(mode_label.to_string(), Some("StatusLineMode")),
        "filename" => one(
            if ctx.file_tail.is_empty() {
                "[No Name]".to_string()
            } else {
                ctx.file_tail.clone()
            },
            None,
        ),
        "filepath" => one(
            if ctx.file_rel.is_empty() {
                "[No Name]".to_string()
            } else {
                ctx.file_rel.clone()
            },
            None,
        ),
        "filetype" => one(ctx.filetype.clone(), None),
        "encoding" => one(ctx.fileencoding.clone(), None),
        "location" => one(format!("{}:{}", ctx.line, ctx.col), None),
        "modified" => one(modified_flag(ctx, "[+]", "[-]"), Some("StatusLineModified")),
        "readonly" => one(if ctx.readonly { "[RO]" } else { "" }.to_string(), None),
        "diagnostics" => diagnostics_cells(ctx),
        _ => return None,
    })
}

/// The `diagnostics` built-in: one cell per non-zero severity, in
/// error→warn→info→hint order, each in its `Diagnostic{Error,Warn,Info,Hint}`
/// highlight group. Empty when the buffer is clean.
fn diagnostics_cells(ctx: &StatuslineCtx) -> Vec<StatusSegment> {
    const SEV: [(&str, &str); 4] = [
        ("E", "DiagnosticError"),
        ("W", "DiagnosticWarn"),
        ("I", "DiagnosticInfo"),
        ("H", "DiagnosticHint"),
    ];
    let mut cells = Vec::new();
    for (i, (sigil, group)) in SEV.iter().enumerate() {
        let n = ctx.diag_counts[i];
        if n > 0 {
            let sep = if cells.is_empty() { "" } else { " " };
            cells.push(StatusSegment {
                text: format!("{sep}{sigil}{n}"),
                group: Some((*group).to_string()),
                on_click: None,
            });
        }
    }
    cells
}

/// Compose a `nx.statusline` [`SegmentLayout`] into the final [`StatusSegment`]s
/// for a status line `width` cells wide.
///
/// Each name resolves built-in first ([`builtin_segment`]), else through
/// `custom` (the server's cache of Lua-published segment cells). A name found in
/// neither renders a visible `E:<name>` cell — loud, never a silent blank, per
/// the no-stub rule. Non-empty segments on the same side are separated by a
/// single space, and a `%=`-style [`Piece::Align`] sits between the two halves,
/// so the existing [`layout`] handles fill and truncation.
pub fn compose_segments(
    spec: &SegmentLayout,
    ctx: &StatuslineCtx,
    mode_label: &str,
    width: usize,
    custom: &dyn Fn(&str) -> Option<Vec<StatusSegment>>,
) -> Vec<StatusSegment> {
    layout(&segment_pieces(spec, ctx, mode_label, custom), width)
}

/// [`compose_segments`], also returning the [`ClickRegion`]s for any cells that
/// carry an [`on_click`](StatusSegment::on_click) handler (an `nx.statusline`
/// segment's `on_click`). The server uses the spans to resolve a status-line click
/// to its handler. Shares the exact piece-building of [`compose_segments`], so the
/// painted line and the click spans always agree.
pub fn compose_segments_with_clicks(
    spec: &SegmentLayout,
    ctx: &StatuslineCtx,
    mode_label: &str,
    width: usize,
    custom: &dyn Fn(&str) -> Option<Vec<StatusSegment>>,
) -> (Vec<StatusSegment>, Vec<ClickRegion>) {
    layout_with_clicks(&segment_pieces(spec, ctx, mode_label, custom), width)
}

/// Build the [`Piece`] stream for a [`SegmentLayout`]: the left side, a `%=`
/// [`Piece::Align`], then the right side. Shared by [`compose_segments`] and
/// [`compose_segments_with_clicks`].
fn segment_pieces(
    spec: &SegmentLayout,
    ctx: &StatuslineCtx,
    mode_label: &str,
    custom: &dyn Fn(&str) -> Option<Vec<StatusSegment>>,
) -> Vec<Piece> {
    let mut pieces: Vec<Piece> = Vec::new();
    push_side(&spec.left, ctx, mode_label, custom, &mut pieces);
    pieces.push(Piece::Align);
    push_side(&spec.right, ctx, mode_label, custom, &mut pieces);
    pieces
}

/// Append one side's resolved segments to `pieces`, space-separated, skipping
/// the empties so they leave no stray separator. A cell carrying an
/// [`on_click`](StatusSegment::on_click) handler is wrapped in a
/// [`Piece::ClickStart`]/`ClickEnd` pair so [`layout_with_clicks`] tracks its
/// column span (the segment analogue of the `%`-format's `%@…%X`).
fn push_side(
    names: &[String],
    ctx: &StatuslineCtx,
    mode_label: &str,
    custom: &dyn Fn(&str) -> Option<Vec<StatusSegment>>,
    pieces: &mut Vec<Piece>,
) {
    let mut wrote = false;
    for name in names {
        let cells = builtin_segment(name, ctx, mode_label)
            .or_else(|| custom(name))
            .unwrap_or_else(|| {
                vec![StatusSegment {
                    text: format!("E:{name}"),
                    group: Some("ErrorMsg".to_string()),
                    on_click: None,
                }]
            });
        if cells.iter().all(|c| c.text.is_empty()) {
            continue;
        }
        // A single space between adjacent non-empty segments (and a leading one,
        // so the bar doesn't butt against the edge).
        push_text(pieces, " ".to_string(), None);
        for cell in cells {
            match cell.on_click {
                Some(handler) => {
                    pieces.push(Piece::ClickStart {
                        action: ClickAction::Handler { handler, minwid: 0 },
                    });
                    push_text(pieces, cell.text, cell.group);
                    pieces.push(Piece::ClickEnd);
                }
                None => push_text(pieces, cell.text, cell.group),
            }
        }
        wrote = true;
    }
    if wrote {
        push_text(pieces, " ".to_string(), None);
    }
}

#[cfg(test)]
mod tests {
    //! Every expected value here is **ground truth captured from real neovim**,
    //! not guessed (per the exception granted in
    //! `docs/plans/2026-06-07-statusline.md` for this dense format language).
    //! The oracle is
    //! `nvim --headless -c 'lua ... nvim_eval_statusline(fmt, {maxwidth=…}) ...'`;
    //! each test cites the format string and the captured `.str`. Re-capture by
    //! evaluating the cited format against the buffer described in [`ctx`].
    use super::*;

    /// The controlled buffer the field expectations are captured against: a
    /// 5-line `rust` file `/tmp/sub/foo.rs`, modified, cursor on line 2 byte
    /// col 3, whole buffer visible. (Mirrors the oracle script's `enew` setup.)
    fn ctx() -> StatuslineCtx {
        StatuslineCtx {
            file_rel: "/tmp/sub/foo.rs".into(),
            file_full: "/tmp/sub/foo.rs".into(),
            file_tail: "foo.rs".into(),
            modified: true,
            modifiable: true,
            readonly: false,
            help: false,
            filetype: "rust".into(),
            fileencoding: "utf-8".into(),
            bomb: false,
            bufnr: 1,
            line: 2,
            line_count: 5,
            col: 3,
            virtcol: 3,
            top_line: 1,
            text_height: 5,
            diag_counts: [0; 4],
        }
    }

    /// Render `fmt` against `ctx` at `width`, returning the concatenated segment
    /// text — the same string `nvim_eval_statusline` returns in `.str`. Panics if
    /// the format contains an expression (use [`render_eval`] for those).
    fn render(fmt: &str, ctx: &StatuslineCtx, width: usize) -> String {
        render_eval(fmt, ctx, width, &mut |_, _| {
            unreachable!("this format has no %{{}}/%! expression")
        })
    }

    fn render_eval(
        fmt: &str,
        ctx: &StatuslineCtx,
        width: usize,
        eval: &mut dyn FnMut(ExprKind, &str) -> String,
    ) -> String {
        let items = parse(fmt).expect("format parses");
        let pieces = expand(&items, ctx, eval);
        layout(&pieces, width)
            .iter()
            .map(|s| s.text.as_str())
            .collect()
    }

    /// Parse + expand + layout, returning the `(text, group)` segments.
    fn segments(fmt: &str, ctx: &StatuslineCtx, width: usize) -> Vec<(String, Option<String>)> {
        let items = parse(fmt).expect("format parses");
        let pieces = expand(&items, ctx, &mut |_, _| unreachable!());
        layout(&pieces, width)
            .into_iter()
            .map(|s| (s.text, s.group))
            .collect()
    }

    // --- Built-in fields, captured one per item from the controlled buffer ---

    #[test]
    fn fields_match_nvim_oracle() {
        let c = ctx();
        // (format, expected nvim_eval_statusline .str)
        let cases = [
            ("%f", "/tmp/sub/foo.rs"),
            ("%F", "/tmp/sub/foo.rs"),
            ("%t", "foo.rs"),
            ("%m", "[+]"),
            ("%M", "+"),
            ("%r", ""),
            ("%h", ""),
            ("%y", "[rust]"),
            ("%n", "1"),
            ("%l", "2"),
            ("%L", "5"),
            ("%c", "3"),
            ("%v", "3"),
            ("%p", "40"),
            ("%P", "All"),
            ("%%", "%"),
            // combinations (maxwidth large enough not to truncate)
            ("%f %l,%c", "/tmp/sub/foo.rs 2,3"),
            ("L%l/%L", "L2/5"),
            ("%y%m", "[rust][+]"),
        ];
        for (fmt, want) in cases {
            assert_eq!(render(fmt, &c, 0), want, "format {fmt:?}");
        }
    }

    #[test]
    fn modified_and_readonly_flags() {
        // Not-modified, not-modifiable buffer: %m=[-], %M=-  (nvim oracle).
        let c = StatuslineCtx {
            modified: false,
            modifiable: false,
            ..ctx()
        };
        assert_eq!(render("%m", &c, 0), "[-]");
        assert_eq!(render("%M", &c, 0), "-");

        // Not-modified, modifiable: both empty.
        let c = StatuslineCtx {
            modified: false,
            ..ctx()
        };
        assert_eq!(render("%m", &c, 0), "");
        assert_eq!(render("%M", &c, 0), "");

        // Read-only flag, help flag.
        let c = StatuslineCtx {
            readonly: true,
            help: true,
            ..ctx()
        };
        assert_eq!(render("%r", &c, 0), "[RO]");
        assert_eq!(render("%h", &c, 0), "[Help]");
    }

    #[test]
    fn no_name_buffer() {
        // A nameless buffer: %f/%F/%t all show "[No Name]" (nvim oracle).
        let c = StatuslineCtx {
            file_rel: "[No Name]".into(),
            file_full: "[No Name]".into(),
            file_tail: "[No Name]".into(),
            ..ctx()
        };
        assert_eq!(render("%f", &c, 0), "[No Name]");
        assert_eq!(render("%F", &c, 0), "[No Name]");
        assert_eq!(render("%t", &c, 0), "[No Name]");
    }

    #[test]
    fn empty_filetype_expands_to_nothing() {
        let c = StatuslineCtx {
            filetype: String::new(),
            ..ctx()
        };
        assert_eq!(render("%y", &c, 0), "");
    }

    #[test]
    fn percentage_p_matches_oracle() {
        // 10-line buffer: %p = line*100/count (nvim oracle: 1->10 … 10->100).
        for (line, want) in [(1usize, "10"), (2, "20"), (5, "50"), (9, "90"), (10, "100")] {
            let c = StatuslineCtx {
                line,
                line_count: 10,
                ..ctx()
            };
            assert_eq!(render("%p", &c, 0), want, "line {line}");
        }
        // Single-line buffer: %p=100.
        let c = StatuslineCtx {
            line: 1,
            line_count: 1,
            ..ctx()
        };
        assert_eq!(render("%p", &c, 0), "100");
    }

    #[test]
    fn alt_percentage_p_top_bot_all_pct() {
        // All: whole buffer fits in the window.
        let c = StatuslineCtx {
            line_count: 5,
            top_line: 1,
            text_height: 10,
            ..ctx()
        };
        assert_eq!(render("%P", &c, 0), "All");
        // Top: scrolled to the top but more below.
        let c = StatuslineCtx {
            line_count: 100,
            top_line: 1,
            text_height: 10,
            ..ctx()
        };
        assert_eq!(render("%P", &c, 0), "Top");
        // Bot: last line visible, lines above.
        let c = StatuslineCtx {
            line_count: 100,
            top_line: 91,
            text_height: 10,
            ..ctx()
        };
        assert_eq!(render("%P", &c, 0), "Bot");
        // Mid: a 3-wide right-justified percentage.
        let c = StatuslineCtx {
            line_count: 100,
            top_line: 41,
            text_height: 10,
            ..ctx()
        };
        // above=40, below=100-(41+10)+1=50 -> 40*100/90 = 44 -> "44%".
        assert_eq!(render("%P", &c, 0), "44%");
    }

    // --- Highlight switches ---

    #[test]
    fn highlight_switch_groups() {
        let c = ctx();
        // `%#Foo#%l%#Bar#x` -> "2" in Foo, "x" in Bar (oracle: two hl runs at 0,1).
        assert_eq!(
            segments("%#Foo#%l%#Bar#x", &c, 0),
            vec![
                ("2".into(), Some("Foo".into())),
                ("x".into(), Some("Bar".into())),
            ]
        );
        // `%1*` selects User1, `%*` / `%0*` reset to the base highlight.
        assert_eq!(
            segments("%1*a%*b", &c, 0),
            vec![("a".into(), Some("User1".into())), ("b".into(), None)]
        );
        assert_eq!(
            segments("%1*a%0*b", &c, 0),
            vec![("a".into(), Some("User1".into())), ("b".into(), None)]
        );
    }

    // --- Alignment (`%=`) — distribution captured from the oracle ---

    #[test]
    fn align_distribution_matches_oracle() {
        let c = ctx();
        assert_eq!(render("a%=b", &c, 20), "a                  b");
        assert_eq!(render("%f%=%l,%c", &c, 20), "/tmp/sub/foo.rs  2,3");
        assert_eq!(render("a%=b%=c", &c, 11), "a    b    c");
        assert_eq!(render("a%=b%=c", &c, 12), "a    b     c"); // last gap takes remainder
        assert_eq!(render("x%=y%=z%=w", &c, 13), "x   y   z   w");
        assert_eq!(render("hello%=", &c, 10), "hello     ");
        assert_eq!(render("%=hello", &c, 10), "     hello");
    }

    // --- Truncation (`%<`) — captured from the oracle ---

    #[test]
    fn truncation_at_marker_matches_oracle() {
        let c = ctx();
        assert_eq!(render("abc%<defghij", &c, 6), "abc<ij");
        assert_eq!(render("abc%<defghij", &c, 8), "abc<ghij");
        assert_eq!(render("abc%<defghij", &c, 100), "abcdefghij"); // fits, no cut
        assert_eq!(render("%<abcdefghij", &c, 5), "<ghij");
        assert_eq!(render("keep%<DROPME", &c, 7), "keep<ME");
        assert_eq!(render("left%<MIDDLE%=right", &c, 12), "left<LEright");
    }

    #[test]
    fn truncation_without_marker_matches_oracle() {
        let c = ctx();
        // No items at all: truncate from the start.
        assert_eq!(render("abcdefghij", &c, 5), "<ghij");
        assert_eq!(render("hello world", &c, 5), "<orld");
        // A `%=` with no `%<`: the first item (the `%=`) is the cut point.
        assert_eq!(render("abc%=xyz", &c, 4), "abc<");
        // Exact fit: unchanged.
        assert_eq!(render("abcde", &c, 5), "abcde");
    }

    #[test]
    fn truncation_prefix_overflow_uses_gt_marker() {
        let c = ctx();
        // Prefix before `%<` already overflows: drop the tail, mark end with `>`.
        assert_eq!(render("abcdef%<gh", &c, 4), "abc>");
        assert_eq!(render("abcdef%<gh", &c, 5), "abcd>");
        assert_eq!(render("LONGPREFIX%<x", &c, 3), "LO>");
    }

    // --- Tabline click regions (`%T` / `%X` / `%nT` / `%nX`) ---

    #[test]
    fn tab_regions_render_nothing() {
        let c = ctx();
        // Bare and numbered tab/close regions carry no visible text; the literals
        // around them survive.
        assert_eq!(render("%1T one %T", &c, 0), " one ");
        assert_eq!(render("%999Xclose", &c, 0), "close");
        assert_eq!(render("%Tab%Xcd", &c, 0), "abcd"); // %T, then ab, then %X, then cd
                                                       // Each region's highlight is set by the surrounding `%#…#`, not the marker.
        assert_eq!(
            segments("%#A#%1T x %#B#%2T y %T%#C#%999Xz", &c, 0),
            vec![
                (" x ".into(), Some("A".into())),
                (" y ".into(), Some("B".into())),
                ("z".into(), Some("C".into())),
            ]
        );
    }

    #[test]
    fn width_prefix_still_errors_but_not_tab_regions() {
        // A digit prefix on a real item is still the unsupported width field…
        assert!(parse("%3l").is_err());
        // …but a digit prefix on T / X is a tab region, not an error.
        assert!(parse("%1T").is_ok());
        assert!(parse("%999X").is_ok());
        assert!(parse("%T").is_ok());
        assert!(parse("%X").is_ok());
    }

    // --- Expressions (the injected eval callback) ---

    #[test]
    fn eval_expression_is_literal_text() {
        let c = ctx();
        let got = render_eval("[%{thing}]", &c, 0, &mut |kind, raw| {
            assert_eq!(kind, ExprKind::Eval);
            assert_eq!(raw, "thing");
            "RESULT".into()
        });
        assert_eq!(got, "[RESULT]");
    }

    #[test]
    fn eval_items_result_is_reparsed() {
        let c = ctx();
        // `%{%…%}` result is re-parsed: it may carry its own fields / hl switches.
        let segs_out = {
            let items = parse("%{%inner%}").unwrap();
            let pieces = expand(&items, &c, &mut |kind, raw| {
                assert_eq!(kind, ExprKind::EvalItems);
                assert_eq!(raw, "inner");
                "%#G#%l".into()
            });
            layout(&pieces, 0)
                .into_iter()
                .map(|s| (s.text, s.group))
                .collect::<Vec<_>>()
        };
        assert_eq!(segs_out, vec![("2".into(), Some("G".into()))]);
    }

    #[test]
    fn whole_statusline_expression() {
        let c = ctx();
        // `%!expr` — the result is the whole statusline, re-parsed.
        let got = render_eval("%!v:lua.foo()", &c, 0, &mut |kind, raw| {
            assert_eq!(kind, ExprKind::Whole);
            assert_eq!(raw, "v:lua.foo()");
            "%f %l".into()
        });
        assert_eq!(got, "/tmp/sub/foo.rs 2");
    }

    // --- Parser error paths (no silent passthrough) ---

    #[test]
    fn unknown_item_errors() {
        assert!(parse("%q").is_err());
        assert!(parse("%3l").is_err()); // width prefix not supported yet
        assert!(parse("%#Unterminated").is_err());
        assert!(parse("%{unterminated").is_err());
    }
}
