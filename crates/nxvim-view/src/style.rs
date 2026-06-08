//! A neutral, frontend-agnostic style and border representation.
//!
//! The server resolves every highlight group to a concrete style and sends it in
//! the `redraw` (see the *View protocol* in `docs/architecture.md`). This crate
//! decodes those into [`Style`] / [`Border`] values that carry no rendering-toolkit
//! types, so each client converts them to its own (a TUI to `ratatui::style::Style`,
//! a GUI to a truecolor fill). Colors are packed `0xRRGGBB` integers; the top byte
//! is unused (the wire never sets it).

/// A resolved style: optional truecolor `fg`/`bg`/`sp` (underline) colors as
/// `0xRRGGBB`, plus the attribute flags. `Default` is "unset everything", so a
/// client patches it cleanly over whatever it paints onto. `undercurl` is kept
/// distinct from `underline` (the server tracks both) even though some clients
/// collapse them when rendering.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Style {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    /// Underline / "special" color (vim's `sp`).
    pub sp: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub strikethrough: bool,
    pub reverse: bool,
}

/// A floating-window border style, mirroring `nvim_win_get_config`'s names. A
/// borderless float / tiled window carries `None` rather than a `Border`, so this
/// enum has no "none" variant. `Solid` is neovim's space border, which a line-based
/// client approximates with its nearest solid glyph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Border {
    Single,
    Rounded,
    Double,
    Solid,
}
