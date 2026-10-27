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
    scale: Option<f32>,
    features: FontFeatures,
}

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
        for (i, spec) in specs.iter().enumerate() {
            match installed_family(font_system, &spec.family) {
                Some(family) => {
                    let font = ChainFont::load(font_system, family, spec);
                    chain.fonts.push(font);
                }
                None => {
                    chain.missing.push(spec.family.clone());
                    if i == 0 {
                        chain.push_monospace(font_system);
                    }
                }
            }
        }
        if specs.is_empty() {
            chain.push_monospace(font_system);
        }
        chain
    }

    fn push_monospace(&mut self, font_system: &mut FontSystem) {
        let name = font_system.db().family_name(&Family::Monospace).to_string();
        if installed_family(font_system, &name).is_some() {
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
        let weight = |w: Option<Weight>| {
            if bold {
                Weight::BOLD
            } else {
                w.unwrap_or(Weight::NORMAL)
            }
        };
        let Some(f) = font.and_then(|i| self.fonts.get(i)) else {
            return attrs.family(self.primary()).weight(weight(None));
        };
        let mut attrs = attrs
            .family(Family::Name(&f.family))
            .weight(weight(f.weight))
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
        let face = font_system
            .db()
            .faces()
            .filter(|f| f.families.iter().any(|(n, _)| *n == family))
            .filter(|f| f.style == fontdb::Style::Normal)
            .min_by_key(|f| f.weight.0.abs_diff(want))
            .map(|f| f.id);
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
            scale: spec.scale,
            features,
        }
    }
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
