//! The configured font list (`'guifont'`), resolved against the font database and
//! applied per character, wezterm's `font_with_fallback`.
//!
//! Each grapheme goes to the first font in the list that can draw it, and is shaped
//! with that font named outright and its own options applied, so the list's order is
//! the user's for every script. A grapheme no listed font covers is left to
//! cosmic-text's platform fallback.

use std::collections::HashSet;

use bemtvi_view::FontSpec;
use glyphon::cosmic_text::{FeatureTag, FontFeatures};
use glyphon::{fontdb, Attrs, Family, FontSystem, Metrics, Weight};
use unicode_segmentation::UnicodeSegmentation;

/// One installed font of the list.
pub(crate) struct ChainFont {
    /// The database's own spelling of the family, which names it when shaping.
    pub(crate) family: String,
    /// The characters its regular face draws.
    coverage: HashSet<u32>,
    weight: Option<Weight>,
    /// The weights its upright faces draw: one per static face, the `wght` axis's range
    /// for a variable one.
    weights: Vec<(u16, u16)>,
    scale: Option<f32>,
    features: FontFeatures,
}

/// The fonts the system monospace means on this platform, best first. Named outright
/// because the database's own idea of it is cosmic-text's hardcoded `Noto Sans Mono` on
/// every platform; where that isn't installed, cosmic-text falls back to the first
/// monospaced face it holds, whatever its style (Courier New Italic on macOS).
const PLATFORM_MONOSPACE: &[&str] = if cfg!(target_os = "macos") {
    &["Menlo", "Monaco"]
} else if cfg!(windows) {
    &["Cascadia Mono", "Consolas", "Courier New"]
} else {
    &["Noto Sans Mono", "DejaVu Sans Mono", "Liberation Mono"]
};

/// Added after the system monospace when it stands in for an unset first font, so the
/// icons plugins draw work out of the box. Not bundled, so only if it's installed.
const DEFAULT_SYMBOLS: &str = "Symbols Nerd Font Mono";

/// The configured list as installed: the fonts the database has, in order, and the
/// names it doesn't. The first entry is the primary, measured for the cell. When the
/// first configured font isn't installed, or none is configured, the system monospace
/// takes its place, so the cell and plain text never come from a symbol or emoji font
/// further down the list.
#[derive(Default)]
pub(crate) struct FontChain {
    pub(crate) fonts: Vec<ChainFont>,
    pub(crate) missing: Vec<String>,
}

/// Characters that ride with the one before them and don't decide a font: the
/// zero-width joiner of an emoji sequence, and the text/emoji variation selectors.
fn decides_font(ch: char) -> bool {
    !matches!(ch, '\u{200D}' | '\u{FE0E}' | '\u{FE0F}')
}

impl FontChain {
    pub(crate) fn resolve(font_system: &mut FontSystem, specs: &[FontSpec]) -> Self {
        let mut chain = Self::default();
        let mut stood_in = specs.is_empty();
        for (i, spec) in specs.iter().enumerate() {
            match installed_family(font_system, &spec.family) {
                Some(family) => {
                    let font = ChainFont::load(font_system, family, spec);
                    chain.fonts.push(font);
                }
                None => {
                    chain.missing.push(spec.family.clone());
                    if i == 0 {
                        stood_in = true;
                        chain.push_monospace(font_system);
                    }
                }
            }
        }
        if specs.is_empty() {
            chain.push_monospace(font_system);
        }
        if stood_in {
            chain.push_default_symbols(font_system);
        }
        chain
    }

    fn push_monospace(&mut self, font_system: &mut FontSystem) {
        if let Some(name) = monospace_family(font_system) {
            let font = ChainFont::load(font_system, name, &FontSpec::default());
            self.fonts.push(font);
        }
    }

    fn push_default_symbols(&mut self, font_system: &mut FontSystem) {
        let Some(name) = installed_family(font_system, DEFAULT_SYMBOLS) else {
            return;
        };
        if !self.fonts.iter().any(|f| f.family == name) {
            let font = ChainFont::load(font_system, name, &FontSpec::default());
            self.fonts.push(font);
        }
    }

    /// The primary family, the one the cell is measured from.
    pub(crate) fn primary(&self) -> Family<'_> {
        self.fonts
            .first()
            .map_or(Family::Monospace, |f| Family::Name(&f.family))
    }

    /// Which listed font draws `cluster`: the first that has every character in it
    /// that decides a font. `None` when none does.
    pub(crate) fn font_for(&self, cluster: &str) -> Option<usize> {
        self.fonts.iter().position(|f| {
            cluster
                .chars()
                .filter(|&c| decides_font(c))
                .all(|c| f.coverage.contains(&(c as u32)))
        })
    }

    /// The weight the primary draws regular text at, the one the cell is measured from.
    pub(crate) fn primary_weight(&self) -> Weight {
        self.fonts.first().map_or(Weight::NORMAL, |f| {
            f.drawable_weight(f.weight.unwrap_or(Weight::NORMAL))
        })
    }

    /// `attrs` for text drawn by listed font `font`: its family named outright, its
    /// weight for regular text (bold stays bold), its features, and its scale on the
    /// size. `None` (no listed font covers the text) names the primary and lets
    /// cosmic-text's fallback find a font, with no font's options applied.
    pub(crate) fn attrs<'a>(
        &'a self,
        font: Option<usize>,
        attrs: Attrs<'a>,
        bold: bool,
        metrics: Metrics,
    ) -> Attrs<'a> {
        let Some(f) = font.and_then(|i| self.fonts.get(i)) else {
            let weight = if bold { Weight::BOLD } else { Weight::NORMAL };
            return attrs.family(self.primary()).weight(weight);
        };
        let weight = if bold {
            Weight::BOLD
        } else {
            f.weight.unwrap_or(Weight::NORMAL)
        };
        let mut attrs = attrs
            .family(Family::Name(&f.family))
            .weight(f.drawable_weight(weight))
            .font_features(f.features.clone());
        if let Some(scale) = f.scale {
            attrs = attrs.metrics(Metrics::new(metrics.font_size * scale, metrics.line_height));
        }
        attrs
    }
}

impl ChainFont {
    fn load(font_system: &mut FontSystem, family: String, spec: &FontSpec) -> Self {
        let weight = spec.weight.map(Weight);
        // The face its regular text uses: upright, at the nearest weight.
        let want = weight.unwrap_or(Weight::NORMAL).0;
        let upright: Vec<(fontdb::ID, u16)> = font_system
            .db()
            .faces()
            .filter(|f| f.families.iter().any(|(n, _)| *n == family))
            .filter(|f| f.style == fontdb::Style::Normal)
            .map(|f| (f.id, f.weight.0))
            .collect();
        let face = upright
            .iter()
            .min_by_key(|(_, w)| w.abs_diff(want))
            .map(|(id, _)| *id);
        let weights = upright
            .iter()
            .map(|&(id, w)| weight_range(font_system.db(), id, w))
            .collect();
        let coverage = face
            .map(|id| coverage(font_system.db(), id))
            .unwrap_or_default();
        let mut features = FontFeatures::new();
        for (tag, value) in &spec.features {
            let bytes = tag.as_bytes();
            if let [a, b, c, d] = *bytes {
                features.set(FeatureTag::new(&[a, b, c, d]), u32::from(*value));
            }
        }
        Self {
            family,
            coverage,
            weight,
            weights,
            scale: spec.scale,
            features,
        }
    }

    /// The weight to ask for to draw at `want` in this font: `want` if it has it, else
    /// the nearest it has. Asking for a weight the family lacks sends cosmic-text to its
    /// fallback list (`.SF NS` first on macOS, which isn't monospaced), not to the
    /// family's nearest face. A font with no bold face draws bold text at its own weight.
    fn drawable_weight(&self, want: Weight) -> Weight {
        self.weights
            .iter()
            .map(|&(low, high)| want.0.clamp(low, high))
            .min_by_key(|w| w.abs_diff(want.0))
            .map_or(want, Weight)
    }
}

/// The weights face `id` draws: its `wght` axis's range if it's variable, else `weight`.
fn weight_range(db: &fontdb::Database, id: fontdb::ID, weight: u16) -> (u16, u16) {
    db.with_face_data(id, |data, index| {
        let face = ttf_parser::Face::parse(data, index).ok()?;
        let axis = face
            .variation_axes()
            .into_iter()
            .find(|a| a.tag == ttf_parser::Tag::from_bytes(b"wght"))?;
        Some((axis.min_value as u16, axis.max_value as u16))
    })
    .flatten()
    .unwrap_or((weight, weight))
}

/// The characters face `id` draws: every code point its Unicode `cmap` subtables map.
/// Read from the font file, not cosmic-text's `Font::unicode_codepoints`, which is
/// filled only for a font that has both GPOS and GSUB tables and is empty for the
/// many that don't.
pub(crate) fn coverage(db: &fontdb::Database, id: fontdb::ID) -> HashSet<u32> {
    db.with_face_data(id, |data, index| {
        let mut set = HashSet::new();
        let Ok(face) = ttf_parser::Face::parse(data, index) else {
            return set;
        };
        if let Some(cmap) = face.tables().cmap {
            for sub in cmap.subtables {
                if sub.is_unicode() {
                    sub.codepoints(|c| {
                        set.insert(c);
                    });
                }
            }
        }
        set
    })
    .unwrap_or_default()
}

/// The database's spelling of `name`, if any face carries it as a family name (in any
/// of its languages), ignoring case.
fn installed_family(font_system: &FontSystem, name: &str) -> Option<String> {
    font_system.db().faces().find_map(|f| {
        f.families
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(n, _)| n.clone())
    })
}

/// The family that stands in as the system monospace: the platform's own, then the one
/// the database names, then any installed monospaced family with an upright face.
fn monospace_family(font_system: &FontSystem) -> Option<String> {
    let db_name = font_system.db().family_name(&Family::Monospace);
    PLATFORM_MONOSPACE
        .iter()
        .copied()
        .chain([db_name])
        .find_map(|name| installed_family(font_system, name))
        .or_else(|| {
            font_system
                .db()
                .faces()
                .filter(|f| f.monospaced && f.style == fontdb::Style::Normal)
                .filter(|f| !f.post_script_name.contains("Emoji"))
                .filter_map(|f| f.families.first().map(|(n, _)| n.clone()))
                .filter(|n| !n.eq_ignore_ascii_case(DEFAULT_SYMBOLS))
                .min()
        })
}

/// Split `text` into maximal runs of grapheme clusters that `key` maps to the same
/// value, pairing each run with it.
pub fn runs_by<'a, T: PartialEq + Copy>(
    text: &'a str,
    key: &dyn Fn(&str) -> T,
) -> Vec<(&'a str, T)> {
    let mut runs: Vec<(&str, T)> = Vec::new();
    let mut start = 0;
    let mut current: Option<T> = None;
    for (i, cluster) in text.grapheme_indices(true) {
        let want = key(cluster);
        match current {
            Some(w) if w == want => {}
            Some(w) => {
                runs.push((&text[start..i], w));
                start = i;
                current = Some(want);
            }
            None => current = Some(want),
        }
    }
    if let Some(w) = current {
        runs.push((&text[start..], w));
    }
    runs
}
