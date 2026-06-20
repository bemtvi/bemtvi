//! The highlight-group registry: nxvim's analogue of neovim's highlight table
//! (`highlight_group.c`).
//!
//! A colorscheme (catppuccin, today) populates this by calling `nvim_set_hl`
//! hundreds of times — each defining a group's foreground/background/special
//! colors and attributes, or *linking* it to another group. This module is the
//! pure, synchronous core of that: a `name -> `[`HlDef`] map plus a resolver
//! that follows link chains to a concrete [`Style`]. It owns no I/O and no Lua;
//! the server parses the Lua opts table, the core just stores and resolves.
//!
//! Per the *Architecture note* in the catppuccin design doc, color now lives in
//! the editor (here), not the client: the server resolves each treesitter
//! capture / chrome region to a concrete style and the client paints it. This
//! module is the resolution half — [`Highlights::resolve_capture`] walks the
//! standard `@`-group fallback chain (`function.call` -> `@function.call` ->
//! `@function` -> `Function`) so a theme that only defines `Function` still
//! colors function calls. The server resolves each capture on redraw and the
//! client paints the resulting style.

use std::collections::HashMap;

/// A 24-bit truecolor value (`#rrggbb`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    /// Pack into the `0xRRGGBB` integer neovim's API reports colors as.
    pub fn to_u32(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | self.b as u32
    }
}

/// A highlight group as defined by one `nvim_set_hl(0, name, opts)` call.
///
/// A group is either a **link** (`link` set — it's a pure alias and its own
/// attributes are ignored, matching neovim) or a set of concrete attributes.
/// catppuccin uses one or the other per group, never both.
#[derive(Clone, Default, Debug, PartialEq)]
pub struct HlDef {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub sp: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub strikethrough: bool,
    pub reverse: bool,
    /// When set, this group is an alias for `link`; its own attrs are ignored.
    pub link: Option<String>,
}

impl HlDef {
    /// Whether this definition carries no usable style (no colors, no attrs, no
    /// link) — the cleared state. Resolving such a group yields `None`.
    fn is_blank(&self) -> bool {
        self.fg.is_none()
            && self.bg.is_none()
            && self.sp.is_none()
            && !self.bold
            && !self.italic
            && !self.underline
            && !self.undercurl
            && !self.strikethrough
            && !self.reverse
            && self.link.is_none()
    }
}

/// A fully resolved concrete style: every link followed, no aliases left. This
/// is what the renderer paints (server resolves, client draws).
#[derive(Clone, Default, Debug, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub sp: Option<Rgb>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub strikethrough: bool,
    pub reverse: bool,
}

impl Style {
    /// A style with nothing set — treated as "no highlight" so capture
    /// resolution falls through to the next candidate group.
    fn is_empty(&self) -> bool {
        self.fg.is_none()
            && self.bg.is_none()
            && self.sp.is_none()
            && !self.bold
            && !self.italic
            && !self.underline
            && !self.undercurl
            && !self.strikethrough
            && !self.reverse
    }

    fn from_def(def: &HlDef) -> Style {
        Style {
            fg: def.fg,
            bg: def.bg,
            sp: def.sp,
            bold: def.bold,
            italic: def.italic,
            underline: def.underline,
            undercurl: def.undercurl,
            strikethrough: def.strikethrough,
            reverse: def.reverse,
        }
    }
}

/// The highlight-group registry. Namespace `0` is the global table a
/// colorscheme populates (`nvim_set_hl(0, …)`) and the renderer resolves
/// against; non-zero namespaces (`nvim_set_hl(ns, …)`, keyed off
/// `nvim_create_namespace`) live in their own tables so a plugin styling a
/// group in *its* namespace never clobbers the global definition. Each table
/// maps group name -> definition; resolution follows links within a table
/// (a non-zero namespace falling back to the global table per hop).
#[derive(Default)]
pub struct Highlights {
    /// The global namespace (`0`) — the table the renderer resolves against.
    groups: HashMap<String, HlDef>,
    /// Per-namespace tables for non-zero namespaces, keyed by namespace id.
    /// Empty in the common single-colorscheme case, so the global path pays
    /// nothing for namespace support.
    namespaces: HashMap<u32, HashMap<String, HlDef>>,
    /// Bumped on every mutation ([`Highlights::set`] / [`Highlights::set_ns`] /
    /// [`Highlights::clear`]), so the server can mirror the tables into Lua only
    /// when they actually changed (the Rust→Lua `nvim_get_hl` mirror is
    /// otherwise hundreds of entries to re-serialize before every Lua chunk).
    /// Wraps harmlessly: only equality against the last-pushed value matters.
    generation: u64,
}

/// Guards link resolution against a cycle (`A -> B -> A`): deep enough for any
/// real theme's link chains, shallow enough to terminate a loop immediately.
const MAX_LINK_DEPTH: usize = 32;

impl Highlights {
    /// A fresh registry: empty until a colorscheme populates it. nxvim *does*
    /// bundle a default scheme (`:colorscheme nxvim`, embedded in the server),
    /// but it is opt-in like any other — nothing is loaded automatically, so the
    /// client's no-theme fallback look governs until a scheme is selected.
    /// `:hi clear` returns here.
    pub fn new() -> Self {
        Highlights::default()
    }

    /// Define (or redefine) a group in the global namespace, as
    /// `nvim_set_hl(0, name, opts)` does. A blank definition (empty opts table)
    /// clears the group, matching neovim.
    pub fn set(&mut self, name: &str, def: HlDef) {
        self.set_ns(0, name, def);
    }

    /// Define (or redefine) a group in namespace `ns`, as
    /// `nvim_set_hl(ns, name, opts)` does. `ns == 0` writes the global table
    /// (identical to [`set`](Self::set)); a non-zero `ns` writes that
    /// namespace's own table, leaving the global definition untouched. A blank
    /// definition clears the group from the target table, matching neovim.
    pub fn set_ns(&mut self, ns: u32, name: &str, def: HlDef) {
        let table = if ns == 0 {
            &mut self.groups
        } else {
            self.namespaces.entry(ns).or_default()
        };
        if def.is_blank() {
            table.remove(name);
        } else {
            table.insert(name.to_string(), def);
        }
        self.generation = self.generation.wrapping_add(1);
    }

    /// `:hi clear` — drop every global group back to the empty default state.
    /// (Non-zero namespaces are left as-is; `:hi clear` is a global operation.)
    pub fn clear(&mut self) {
        self.groups.clear();
        self.generation = self.generation.wrapping_add(1);
    }

    /// The raw (unresolved) definition for `name` in the global namespace, if
    /// any. `link` is still present — callers wanting a concrete style use
    /// [`Highlights::resolve`].
    pub fn get(&self, name: &str) -> Option<&HlDef> {
        self.groups.get(name)
    }

    /// A change counter bumped on every [`set`](Self::set) / [`clear`](Self::clear).
    /// The server gates the Rust→Lua `nvim_get_hl` mirror push on it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Iterate the raw `(name, def)` definitions of the global namespace — the
    /// source the server folds into the Lua `nx._hl_defs` mirror that backs
    /// `nvim_get_hl`. Links are kept unresolved (the Lua side follows the chain
    /// when asked).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &HlDef)> {
        self.groups.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate `(ns, name, def)` over every non-zero namespace's raw
    /// definitions — the source for the per-namespace Lua mirror tables. The
    /// global namespace is excluded (it has its own [`iter`](Self::iter)).
    pub fn iter_namespaces(&self) -> impl Iterator<Item = (u32, &str, &HlDef)> {
        self.namespaces
            .iter()
            .flat_map(|(&ns, table)| table.iter().map(move |(k, v)| (ns, k.as_str(), v)))
    }

    /// Resolve a group in the global namespace to a concrete [`Style`].
    /// Equivalent to [`resolve_ns(0, name)`](Self::resolve_ns).
    pub fn resolve(&self, name: &str) -> Option<Style> {
        self.resolve_ns(0, name)
    }

    /// Resolve a group within namespace `ns` to a concrete [`Style`], following
    /// its link chain (cycle-guarded). Returns `None` when the group is absent,
    /// cleared, or links to a dead end — i.e. when it contributes no highlight.
    ///
    /// For a non-zero `ns` the lookup is rooted in that namespace's table: a
    /// group not defined there yields `None` (matching neovim's
    /// `nvim_get_hl(ns, …)`, which returns the namespace's own table — render
    /// time falls back to the global table separately). Once rooted, each link
    /// hop is looked up in the namespace first, then the global table, so a
    /// namespaced alias of a global base group still resolves.
    pub fn resolve_ns(&self, ns: u32, name: &str) -> Option<Style> {
        if ns != 0
            && !self
                .namespaces
                .get(&ns)
                .is_some_and(|t| t.contains_key(name))
        {
            return None;
        }
        let ns_table = (ns != 0).then(|| self.namespaces.get(&ns)).flatten();
        let lookup = |n: &str| {
            ns_table
                .and_then(|t| t.get(n))
                .or_else(|| self.groups.get(n))
        };
        let mut current = name;
        for _ in 0..MAX_LINK_DEPTH {
            let def = lookup(current)?;
            if let Some(link) = &def.link {
                current = link;
                continue;
            }
            let style = Style::from_def(def);
            return if style.is_empty() { None } else { Some(style) };
        }
        None // cycle or pathologically deep chain
    }

    /// Resolve a treesitter capture name (`function.call`, `string`, …) to a
    /// concrete style by walking the standard fallback chain: the `@`-group at
    /// progressively shorter specificity, then the legacy syntax group. The
    /// first candidate that resolves wins; an unknown capture yields `None`.
    ///
    /// `function.call` -> `@function.call` -> `@function` -> `Function`.
    pub fn resolve_capture(&self, capture: &str) -> Option<Style> {
        capture_fallbacks(capture)
            .into_iter()
            .find_map(|group| self.resolve(&group))
    }
}

/// A window-local `winhighlight` remap (vim's `'winhighlight'` / `'winhl'`): an
/// ordered list of `from -> to` highlight-group renames applied while rendering a
/// single window. When a window carries `Normal:NormalSB,EndOfBuffer:Hidden`,
/// every place that would resolve the group `Normal` in that window resolves
/// `NormalSB` instead, and `EndOfBuffer` resolves `Hidden`.
///
/// The remap is **one level**: [`remap`](Self::remap) substitutes the target name
/// once, and the caller hands that name to [`Highlights::resolve`], which then
/// follows the target's *own* `link` chain — but a second `winhighlight` pair is
/// never chained onto the first (`Normal:A,A:B` leaves `Normal` resolving `A`,
/// matching vim). Unknown group names on either side are kept verbatim and simply
/// fail to match anything at resolve time (also vim-faithful); only *syntactically*
/// malformed entries are dropped, and [`parse_reporting`](Self::parse_reporting)
/// hands those back so the setter can warn rather than silently ignore them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WinHl {
    /// `(from, to)` pairs in declaration order. Small (a handful of entries at
    /// most), so a linear scan in [`remap`](Self::remap) is cheaper than a map.
    pairs: Vec<(String, String)>,
}

impl WinHl {
    /// Parse a `'winhighlight'` value (`"Normal:NormalSB,SignColumn:NormalSB"`),
    /// returning the remap plus every malformed entry (one with no `:` or an empty
    /// side) for the caller to report. Well-formed pairs are kept in declaration
    /// order; a later pair for the same `from` wins ([`remap`](Self::remap) reads
    /// from the end). An empty/whitespace value parses to an empty remap.
    pub fn parse_reporting(s: &str) -> (Self, Vec<String>) {
        let mut pairs = Vec::new();
        let mut bad = Vec::new();
        for entry in s.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match entry.split_once(':') {
                Some((from, to)) if !from.is_empty() && !to.is_empty() => {
                    pairs.push((from.to_string(), to.to_string()));
                }
                _ => bad.push(entry.to_string()),
            }
        }
        (WinHl { pairs }, bad)
    }

    /// Parse a `'winhighlight'` value, discarding the malformed-entry report — for
    /// the render path, where the setter has already validated and warned.
    pub fn parse(s: &str) -> Self {
        Self::parse_reporting(s).0
    }

    /// Whether this remap renames nothing (the common case — most windows).
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// The group name to resolve in place of `group`, or `group` unchanged when no
    /// pair renames it. One level only (see the type docs); the last matching pair
    /// wins.
    pub fn remap<'a>(&'a self, group: &'a str) -> &'a str {
        self.pairs
            .iter()
            .rev()
            .find(|(from, _)| from == group)
            .map_or(group, |(_, to)| to.as_str())
    }
}

/// Parse a color as written in an `nvim_set_hl` opts table: a `#rrggbb` literal,
/// a small set of named colors, or `"NONE"`. Returns `None` for `NONE` and for
/// anything unrecognized (both mean "no color here").
pub fn parse_color(spec: &str) -> Option<Rgb> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case("none") || spec.is_empty() {
        return None;
    }
    if let Some(hex) = spec.strip_prefix('#') {
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Rgb { r, g, b });
        }
        return None;
    }
    named_color(spec)
}

/// The handful of named colors a theme might reference (the standard ANSI
/// names). catppuccin's compiled output is all hex, so this is a safety net for
/// the few groups that use a name; unknown names resolve to `None`.
fn named_color(name: &str) -> Option<Rgb> {
    let rgb = |r, g, b| Some(Rgb { r, g, b });
    match name.to_ascii_lowercase().as_str() {
        "black" => rgb(0, 0, 0),
        "white" => rgb(255, 255, 255),
        "red" => rgb(255, 0, 0),
        "green" => rgb(0, 128, 0),
        "blue" => rgb(0, 0, 255),
        "yellow" => rgb(255, 255, 0),
        "cyan" => rgb(0, 255, 255),
        "magenta" => rgb(255, 0, 255),
        "gray" | "grey" => rgb(128, 128, 128),
        _ => None,
    }
}

/// Build the ordered candidate groups for a capture name. For `a.b.c`:
/// `@a.b.c`, `@a.b`, `@a`, then the legacy syntax group for the major segment
/// (`a`) if one exists. This mirrors neovim's treesitter default-link fallback,
/// so a theme styling only the broad group still colors specific captures.
fn capture_fallbacks(capture: &str) -> Vec<String> {
    let parts: Vec<&str> = capture.split('.').collect();
    let mut out = Vec::with_capacity(parts.len() + 1);
    for i in (1..=parts.len()).rev() {
        out.push(format!("@{}", parts[..i].join(".")));
    }
    if let Some(legacy) = legacy_group(parts[0]) {
        out.push(legacy.to_string());
    }
    out
}

/// Map a capture's major segment to the legacy syntax group that themes have
/// always styled (`Comment`, `Function`, `Keyword`, …). The terminal fallback
/// in [`capture_fallbacks`]: covers the captures nxvim-ts emits, so even a
/// minimal theme that only sets legacy groups colors the buffer.
fn legacy_group(major: &str) -> Option<&'static str> {
    Some(match major {
        "comment" => "Comment",
        "string" => "String",
        "character" => "Character",
        "number" => "Number",
        "boolean" => "Boolean",
        "float" => "Float",
        "constant" => "Constant",
        "function" | "method" | "constructor" => "Function",
        "keyword" | "keyword_operator" => "Keyword",
        "conditional" => "Conditional",
        "repeat" => "Repeat",
        "include" => "Include",
        "exception" => "Exception",
        "type" | "namespace" | "module" => "Type",
        "operator" => "Operator",
        "punctuation" => "Delimiter",
        "property" | "field" | "attribute" | "variable" => "Identifier",
        "label" => "Label",
        "tag" => "Tag",
        _ => return None,
    })
}
