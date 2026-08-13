//! LSP snippet grammar — parsing snippet bodies into a flat tabstop model.
//!
//! This is the pure, synchronous front half of the native snippet engine: it
//! turns an LSP snippet body (`fn ${1:name}(${2}) -> ${3:()} {\n\t$0\n}`) into the
//! literal text to insert plus the byte ranges of every tabstop within it. The
//! stateful session that drives the cursor through those tabstops lives in
//! [`crate::editor::snippet`].
//!
//! Grammar (the [LSP snippet syntax]): `$N` / `${N}` tabstops, `${N:default}`
//! placeholders (whose default may itself contain nested tabstops), `$0` the final
//! cursor stop, `${N|a,b,c|}` choices, and **mirrors** — the same `N` appearing
//! more than once, every occurrence kept in sync. `\$`, `\}`, `\\` are the escapes.
//!
//! Per the project's no-silent-stubs rule, anything outside that set — variables
//! (`$TM_FILENAME`), variable placeholders, and `${1/regex/fmt/opts}` transforms —
//! is rejected loud with [`SnippetError::Unsupported`] rather than silently
//! inserted as raw text.
//!
//! [LSP snippet syntax]: https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#snippet_syntax

use std::collections::BTreeMap;
use std::ops::Range;

/// A parsed snippet body: the literal text to insert, plus every tabstop's byte
/// ranges within that text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSnippet {
    /// The text to insert into the buffer, with all placeholder defaults filled
    /// in and every `$…` marker resolved away.
    pub text: String,
    /// The tabstops, in ascending index order with the final stop (`$0`) last.
    /// Empty when the body had no tabstops at all.
    pub stops: Vec<TabStop>,
}

/// One tabstop and all the places it appears in [`ParsedSnippet::text`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStop {
    /// The tabstop number. `0` is the final cursor stop (`$0`), always visited
    /// last regardless of how many higher-numbered stops precede it.
    pub index: u32,
    /// Byte ranges into [`ParsedSnippet::text`]. `spans[0]` is the primary
    /// (editable) occurrence; any further spans are mirrors kept in sync with it.
    /// Sorted by start offset, so `spans[0]` is the first occurrence in the text.
    pub spans: Vec<Range<usize>>,
    /// The choice alternatives for a `${N|a,b,c|}` choice stop, else empty. The
    /// first choice is the default text rendered into [`ParsedSnippet::text`].
    pub choices: Vec<String>,
}

/// Why a snippet body could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnippetError {
    /// A construct outside the supported subset (a variable like `$TM_FILENAME`,
    /// a variable placeholder, or a `${1/regex/fmt/}` transform). Carries a short
    /// description of what was seen, for a loud runtime error.
    Unsupported(String),
    /// Malformed syntax: an unterminated `${…}`, a non-numeric tabstop, etc.
    Malformed(String),
}

impl std::fmt::Display for SnippetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnippetError::Unsupported(s) => write!(f, "unsupported snippet construct: {s}"),
            SnippetError::Malformed(s) => write!(f, "malformed snippet: {s}"),
        }
    }
}

impl std::error::Error for SnippetError {}

/// One element of a parsed snippet body, before the text and spans are laid out.
enum Node {
    /// Literal text (escapes already resolved).
    Text(String),
    /// A tabstop / placeholder / choice: its number, the nested body of a
    /// placeholder default (empty for a bare `$N`), and choice alternatives.
    Stop {
        index: u32,
        body: Vec<Node>,
        choices: Vec<String>,
    },
}

/// Parse an LSP snippet body. Returns the literal insert text and the tabstop
/// ranges, or a [`SnippetError`] for malformed or unsupported input.
pub fn parse_snippet(src: &str) -> Result<ParsedSnippet, SnippetError> {
    let bytes = src.as_bytes();
    let mut pos = 0;
    let nodes = parse_nodes(bytes, &mut pos, false)?;
    if pos != bytes.len() {
        // parse_nodes only stops early on an unescaped `}`, which is illegal at
        // the top level.
        return Err(SnippetError::Malformed("unexpected `}`".into()));
    }

    // Lay the nodes out into the final text, recording each tabstop occurrence's
    // span. `defaults` resolves mirrors: the first occurrence that carries a body
    // (or choices) defines the text every occurrence of that index renders.
    // First resolve each index's default text — the literal a bodied/choice
    // occurrence renders — so that bare `$N` mirrors render the same content rather
    // than an empty span.
    let mut defaults: BTreeMap<u32, String> = BTreeMap::new();
    resolve_defaults(&nodes, &mut defaults);

    let mut text = String::new();
    let mut spans: BTreeMap<u32, Vec<Range<usize>>> = BTreeMap::new();
    let mut choices: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    layout(&nodes, &defaults, &mut text, &mut spans, &mut choices);

    let mut stops: Vec<TabStop> = spans
        .into_iter()
        .map(|(index, mut spans)| {
            spans.sort_by_key(|r| r.start);
            TabStop {
                index,
                spans,
                choices: choices.remove(&index).unwrap_or_default(),
            }
        })
        .collect();
    // Tab order: ascending number, but the final `$0` stop comes last.
    stops.sort_by_key(|s| if s.index == 0 { u32::MAX } else { s.index });

    Ok(ParsedSnippet { text, stops })
}

/// Parse a run of nodes until end-of-input (`in_brace == false`) or an unescaped
/// `}` (`in_brace == true`, leaving `pos` at the `}`).
fn parse_nodes(bytes: &[u8], pos: &mut usize, in_brace: bool) -> Result<Vec<Node>, SnippetError> {
    let mut nodes = Vec::new();
    let mut lit = String::new();
    while *pos < bytes.len() {
        let c = bytes[*pos];
        match c {
            b'}' if in_brace => break,
            b'\\' if *pos + 1 < bytes.len() => {
                // Per the spec only `$`, `}`, `\` are escapable; any other `\x`
                // keeps the backslash literally.
                let n = bytes[*pos + 1];
                if matches!(n, b'$' | b'}' | b'\\') {
                    lit.push(n as char);
                    *pos += 2;
                } else {
                    lit.push('\\');
                    *pos += 1;
                }
            }
            b'$' => {
                if !lit.is_empty() {
                    nodes.push(Node::Text(std::mem::take(&mut lit)));
                }
                nodes.push(parse_dollar(bytes, pos)?);
            }
            _ => {
                // Copy one UTF-8 char (the body is valid UTF-8 from a &str).
                let ch_len = utf8_len(c);
                lit.push_str(
                    std::str::from_utf8(&bytes[*pos..*pos + ch_len])
                        .expect("valid utf-8 slice from &str"),
                );
                *pos += ch_len;
            }
        }
    }
    if !lit.is_empty() {
        nodes.push(Node::Text(lit));
    }
    Ok(nodes)
}

/// Parse a `$…` construct, with `*pos` at the `$`.
fn parse_dollar(bytes: &[u8], pos: &mut usize) -> Result<Node, SnippetError> {
    debug_assert_eq!(bytes[*pos], b'$');
    *pos += 1;
    if *pos >= bytes.len() {
        return Err(SnippetError::Malformed("trailing `$`".into()));
    }
    if bytes[*pos] == b'{' {
        *pos += 1;
        return parse_braced(bytes, pos);
    }
    // `$N` — a bare tabstop. `$name` is a variable (unsupported).
    if bytes[*pos].is_ascii_digit() {
        let index = parse_int(bytes, pos);
        Ok(Node::Stop {
            index,
            body: Vec::new(),
            choices: Vec::new(),
        })
    } else {
        let name = read_ident(bytes, pos);
        Err(SnippetError::Unsupported(format!("variable `${name}`")))
    }
}

/// Parse the inside of a `${…}`, with `*pos` just past the `{`.
fn parse_braced(bytes: &[u8], pos: &mut usize) -> Result<Node, SnippetError> {
    if *pos >= bytes.len() || !bytes[*pos].is_ascii_digit() {
        // `${name}` / `${name:…}` / `${name/…}` — all variable forms.
        let name = read_ident(bytes, pos);
        return Err(SnippetError::Unsupported(format!(
            "variable placeholder `${{{name}…}}`"
        )));
    }
    let index = parse_int(bytes, pos);
    match bytes.get(*pos) {
        Some(b'}') => {
            *pos += 1;
            Ok(Node::Stop {
                index,
                body: Vec::new(),
                choices: Vec::new(),
            })
        }
        Some(b':') => {
            *pos += 1;
            let body = parse_nodes(bytes, pos, true)?;
            expect_close(bytes, pos)?;
            Ok(Node::Stop {
                index,
                body,
                choices: Vec::new(),
            })
        }
        Some(b'|') => {
            *pos += 1;
            let choices = parse_choices(bytes, pos)?;
            Ok(Node::Stop {
                index,
                body: Vec::new(),
                choices,
            })
        }
        Some(b'/') => Err(SnippetError::Unsupported(format!(
            "transform on tabstop ${index}"
        ))),
        _ => Err(SnippetError::Malformed(format!(
            "unterminated `${{{index}…}}`"
        ))),
    }
}

/// Parse `a,b,c|}` of a choice stop, with `*pos` just past the opening `|`.
fn parse_choices(bytes: &[u8], pos: &mut usize) -> Result<Vec<String>, SnippetError> {
    let mut choices = Vec::new();
    let mut cur = String::new();
    while *pos < bytes.len() {
        let c = bytes[*pos];
        match c {
            b'\\' if *pos + 1 < bytes.len() => {
                // Inside a choice, `,` and `|` are escapable too. Any other `\x`
                // keeps the backslash literally and leaves `x` to the normal UTF-8
                // arm below — consuming 2 bytes here would split a multibyte char
                // (the lead byte pushed raw as a `char`, `*pos` landing mid-char,
                // where a continuation byte's `utf8_len` of 4 can slice OOB).
                let n = bytes[*pos + 1];
                if matches!(n, b',' | b'|' | b'\\') {
                    cur.push(n as char);
                    *pos += 2;
                } else {
                    cur.push('\\');
                    *pos += 1;
                }
            }
            b',' => {
                choices.push(std::mem::take(&mut cur));
                *pos += 1;
            }
            b'|' => {
                *pos += 1;
                choices.push(cur);
                expect_close(bytes, pos)?;
                return Ok(choices);
            }
            _ => {
                let ch_len = utf8_len(c);
                cur.push_str(
                    std::str::from_utf8(&bytes[*pos..*pos + ch_len])
                        .expect("valid utf-8 slice from &str"),
                );
                *pos += ch_len;
            }
        }
    }
    Err(SnippetError::Malformed(
        "unterminated choice `${N|…|}`".into(),
    ))
}

/// Consume a required closing `}`.
fn expect_close(bytes: &[u8], pos: &mut usize) -> Result<(), SnippetError> {
    if bytes.get(*pos) == Some(&b'}') {
        *pos += 1;
        Ok(())
    } else {
        Err(SnippetError::Malformed("expected `}`".into()))
    }
}

/// Pre-pass: the literal default text each index renders. The first occurrence
/// that carries a body (or choices) defines it; bare `$N` occurrences inherit it.
fn resolve_defaults(nodes: &[Node], defaults: &mut BTreeMap<u32, String>) {
    for node in nodes {
        if let Node::Stop {
            index,
            body,
            choices,
        } = node
        {
            if !choices.is_empty() {
                defaults.entry(*index).or_insert_with(|| choices[0].clone());
            } else if !body.is_empty() {
                let mut s = String::new();
                render_default(body, defaults, &mut s);
                defaults.entry(*index).or_insert(s);
            }
            resolve_defaults(body, defaults);
        }
    }
}

/// Render a node list to a plain string (no span recording) for default resolution.
fn render_default(nodes: &[Node], defaults: &BTreeMap<u32, String>, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(s) => out.push_str(s),
            Node::Stop {
                index,
                body,
                choices,
            } => {
                if !choices.is_empty() {
                    out.push_str(&choices[0]);
                } else if !body.is_empty() {
                    render_default(body, defaults, out);
                } else if let Some(d) = defaults.get(index) {
                    out.push_str(d);
                }
            }
        }
    }
}

/// Walk the node tree, appending text and recording each stop's span. A bare `$N`
/// (no body / choices) renders the resolved default for its index, so mirrors show
/// the same text as the bodied occurrence.
fn layout(
    nodes: &[Node],
    defaults: &BTreeMap<u32, String>,
    text: &mut String,
    spans: &mut BTreeMap<u32, Vec<Range<usize>>>,
    choices: &mut BTreeMap<u32, Vec<String>>,
) {
    for node in nodes {
        match node {
            Node::Text(s) => text.push_str(s),
            Node::Stop {
                index,
                body,
                choices: ch,
            } => {
                let start = text.len();
                if !ch.is_empty() {
                    // A choice renders its first alternative as the default text.
                    text.push_str(&ch[0]);
                    choices.entry(*index).or_insert_with(|| ch.clone());
                } else if !body.is_empty() {
                    layout(body, defaults, text, spans, choices);
                } else if let Some(d) = defaults.get(index) {
                    // A bare mirror of a bodied occurrence renders that default text.
                    text.push_str(d);
                }
                let end = text.len();
                spans.entry(*index).or_default().push(start..end);
            }
        }
    }
}

/// Read a run of ASCII digits as a `u32` (saturating), advancing `*pos`.
fn parse_int(bytes: &[u8], pos: &mut usize) -> u32 {
    let mut n: u32 = 0;
    while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add((bytes[*pos] - b'0') as u32);
        *pos += 1;
    }
    n
}

/// Read an identifier (`[A-Za-z_][A-Za-z0-9_]*`) for an error message, advancing
/// `*pos`. Used only on the unsupported-variable paths.
fn read_ident(bytes: &[u8], pos: &mut usize) -> String {
    let start = *pos;
    while *pos < bytes.len() && (bytes[*pos].is_ascii_alphanumeric() || bytes[*pos] == b'_') {
        *pos += 1;
    }
    String::from_utf8_lossy(&bytes[start..*pos]).into_owned()
}

/// Byte length of the UTF-8 char whose lead byte is `b`.
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}
