//! The client's own configuration, a purely client concern: the fonts, their size, how
//! a wide glyph is fit, and the modeless caret's blink.

use std::time::Duration;

/// Each half of a modeless caret's blink: shown this long, then hidden as long.
pub const CARET_BLINK: Duration = Duration::from_millis(530);

/// Where a caret's blink stands `elapsed` after it last restarted (the caret moved,
/// or appeared): whether it is in the hidden half, and how long until it flips. It
/// starts shown, so a caret is always visible the moment it moves.
pub fn caret_blink(elapsed: Duration) -> (bool, Duration) {
    let half = CARET_BLINK.as_millis();
    let ms = elapsed.as_millis();
    let hidden = (ms / half) % 2 == 1;
    let left = half - ms % half;
    (hidden, Duration::from_millis(left as u64))
}

/// When a single-cell glyph may be drawn bigger than the one cell the editor reserved
/// for it, spilling its ink into the cell to its right.
///
/// A Nerd Font / symbol glyph is designed square — a full em wide — where a coding
/// font's cell is only ~0.6 em, so fitting it to its reserved cell (what the renderer
/// does by default; see [`crate::cluster_scale`]) shrinks it to about 60% and an icon
/// beside a filename looks undersized. But drawing it at its natural size paints over
/// whatever is in the next cell. The resolution is the neighbour: when the next cell is
/// blank there is nothing to paint over, so the glyph can have that space and render at
/// full size. This is wezterm's `allow_square_glyphs_to_overflow_width`, same three
/// modes and same default.
///
/// The column model never changes — the glyph still *occupies* one cell, so the cursor,
/// selections, and every column the server computes are untouched. Only the ink grows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GlyphOverflow {
    /// Never: every glyph is shrunk to the cells the editor reserved for it.
    Never,
    /// Only when the character to the right is a space, so the ink spills over blank
    /// background and can't collide with a neighbouring glyph. The default.
    #[default]
    WhenFollowedBySpace,
    /// Always, whatever follows — a glyph in the next cell is overpainted.
    Always,
}

impl GlyphOverflow {
    /// Parse a mode name, however it was spelled: the `--glyph-overflow` flag, the
    /// `BEMTVI_GUI_GLYPH_OVERFLOW` environment variable, or the `'guiglyphoverflow'`
    /// option relayed off the redraw. `never`, `always`, or `when-followed-by-space`
    /// (`space` for short), case- and separator-insensitive (`WhenFollowedBySpace` and
    /// `when_followed_by_space` both parse). `None` for anything else, which the caller
    /// reports rather than silently ignoring.
    ///
    /// The core validates `:set guiglyphoverflow=…` against this same set
    /// (`bemtvi-core/src/editor/options.rs`), and the web client re-spells it in
    /// `glyphOverflowMode`; keep the three in step.
    pub fn parse(spec: &str) -> Option<Self> {
        match spec.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "never" => Some(Self::Never),
            "always" => Some(Self::Always),
            "space" | "when-followed-by-space" | "whenfollowedbyspace" => {
                Some(Self::WhenFollowedBySpace)
            }
            _ => None,
        }
    }

    /// The accepted spellings, for a CLI/env error message.
    pub const NAMES: &'static str = "never | always | when-followed-by-space";
}

/// Client-side rendering configuration. The font family and point size are a
/// purely client concern — the server works in cells, not pixels — so they're set
/// here (CLI flags / environment), as well as through `'guifont'`, which wins. A font
/// that isn't installed is reported on the message line and skipped; a missing first
/// font gives its place to the system monospace.
#[derive(Clone, Debug)]
pub struct GuiConfig {
    /// Ordered fonts: the first is the primary, the rest a wezterm-style fallback
    /// chain tried in turn for a glyph the primary lacks. Empty uses the system
    /// monospace. Set from `--font` / `BEMTVI_GUI_FONT`, in `'guifont'`'s list syntax.
    pub fonts: Vec<bemtvi_view::FontSpec>,
    /// Font point size, before the display's scale factor is applied.
    pub font_size: f32,
    /// *Maximum* render scale for an emoji / wide fallback glyph relative to the text
    /// cell — a color-emoji font draws smaller than its reserved cells, so this sizes it
    /// up. It is a ceiling, not a constant: a glyph whose ink is *wider* than the cells
    /// the editor reserved (a Nerd Font icon is a full em where a coding font's cell is
    /// ~0.6 em) is shrunk to fit instead, so it can't collide with the next character.
    /// Set from `--emoji-scale` / `BEMTVI_GUI_EMOJI_SCALE`.
    pub emoji_scale: f32,
    /// Whether a square single-cell glyph (a Nerd Font icon) may be drawn at its natural
    /// size, overflowing into the cell to its right, instead of being shrunk to fit its
    /// own. See [`GlyphOverflow`]. Set from `--glyph-overflow` /
    /// `BEMTVI_GUI_GLYPH_OVERFLOW`.
    pub glyph_overflow: GlyphOverflow,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            fonts: Vec::new(),
            font_size: 13.0,
            emoji_scale: 1.2,
            glyph_overflow: GlyphOverflow::default(),
        }
    }
}

impl GuiConfig {
    /// Overrides from the environment: `BEMTVI_GUI_FONT` (family, or a comma-separated
    /// fallback list) and `BEMTVI_GUI_FONT_SIZE` (points). Absent/blank/invalid values
    /// keep the default; CLI flags layered on top take precedence (see `main`).
    pub fn from_env() -> Self {
        let mut c = Self::default();
        if let Some(name) = std::env::var_os("BEMTVI_GUI_FONT") {
            c.set_font(&name.to_string_lossy());
        }
        if let Ok(size) = std::env::var("BEMTVI_GUI_FONT_SIZE") {
            if let Ok(pt) = size.trim().parse::<f32>() {
                c.set_font_size(pt);
            }
        }
        if let Ok(scale) = std::env::var("BEMTVI_GUI_EMOJI_SCALE") {
            if let Ok(s) = scale.trim().parse::<f32>() {
                c.set_emoji_scale(s);
            }
        }
        if let Ok(mode) = std::env::var("BEMTVI_GUI_GLYPH_OVERFLOW") {
            // A mode name is all-or-nothing — unlike a size, there is nothing sensible to
            // clamp a typo to — so an unrecognised one says so rather than quietly
            // rendering in a mode the user didn't ask for.
            match GlyphOverflow::parse(&mode) {
                Some(m) => c.glyph_overflow = m,
                None => eprintln!(
                    "bemtvi-gui: ignoring BEMTVI_GUI_GLYPH_OVERFLOW={mode:?} \
                     (expected {})",
                    GlyphOverflow::NAMES
                ),
            }
        }
        c
    }

    /// Set the font list from a spec in `'guifont'`'s syntax (`"JetBrains Mono,Noto
    /// Color Emoji"`, per-font options in brackets; see `bemtvi_core::guifont`). An
    /// all-blank spec leaves the list untouched (keeping the monospace default rather
    /// than asking the font system for `""`); one that doesn't parse says why on
    /// stderr and leaves it too.
    pub fn set_font(&mut self, spec: &str) {
        match bemtvi_core::guifont::parse(spec) {
            Ok(g) if !g.fonts.is_empty() => self.fonts = view_fonts(&g),
            Ok(_) => {}
            Err(why) => eprintln!("bemtvi-gui: ignoring font {spec:?}: {why}"),
        }
    }

    /// Set the point size, clamped to `[4, 200]` so a typo can't produce a zero,
    /// negative, or absurd cell. A non-finite or non-positive value is ignored.
    pub fn set_font_size(&mut self, pt: f32) {
        if pt.is_finite() && pt > 0.0 {
            self.font_size = pt.clamp(4.0, 200.0);
        }
    }

    /// Set the emoji render scale *ceiling*, clamped to `[0.25, 4.0]` so a typo can't
    /// blow a glyph up across the screen or vanish it. A non-finite or non-positive
    /// value is ignored (keeps the current scale).
    pub fn set_emoji_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.emoji_scale = scale.clamp(0.25, 4.0);
        }
    }
}

/// The GUI's copy of a list the core parsed (`--font`, `BEMTVI_GUI_FONT`), in the
/// type the relayed `'guifont'` arrives as.
pub fn view_fonts(g: &bemtvi_core::guifont::GuiFont) -> Vec<bemtvi_view::FontSpec> {
    g.fonts
        .iter()
        .map(|f| bemtvi_view::FontSpec {
            family: f.family.clone(),
            weight: f.weight,
            scale: f.scale,
            features: f
                .features
                .iter()
                .map(|(tag, v)| (String::from_utf8_lossy(tag).into_owned(), *v))
                .collect(),
        })
        .collect()
}
