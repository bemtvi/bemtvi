//! The `'guifont'` syntax: a font list tried in order for every character, each
//! font with optional options of its own, then the vim/Neovide `:` flags.
//!
//! ```text
//! JetBrains Mono[weight=500;features=+ss01 -calt],Symbols Nerd Font[scale=0.9]:h14
//! ```
//!
//! The core owns the syntax so a malformed value fails loud on `:set` and every
//! client gets the same parsed list; the fonts themselves are the clients' concern.
//! A backslash-escaped space (the `:set` form) is a space. Options go in brackets
//! after a family, `;`-separated: `weight` (1–1000, or a name such as `bold`),
//! `scale` (a factor in (0, 4]) and `features` (space-separated `+tag`, `-tag`, `tag`
//! or `tag=N`, each tag four characters). After the list, `:h<size>` sets the size in
//! points; other flags (`:b`, `:i`, `:w…`, `:#e-…`) are accepted and ignored, as in
//! Neovide.

/// A parsed `'guifont'`. An empty list means the client's own default font.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuiFont {
    pub fonts: Vec<FontSpec>,
    /// `:h<size>`, in points.
    pub size: Option<f32>,
}

/// One font in the list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontSpec {
    pub family: String,
    /// The weight its regular text uses (bold text stays bold).
    pub weight: Option<u16>,
    /// A factor on its glyphs' size.
    pub scale: Option<f32>,
    /// OpenType features, as `(tag, value)`: `+tag` and `tag` are 1, `-tag` is 0.
    pub features: Vec<([u8; 4], u16)>,
}

/// Parse a `'guifont'` value, or say what is wrong with it.
pub fn parse(value: &str) -> Result<GuiFont, String> {
    let parts = split_top(value, ':');
    let mut parts = parts.into_iter();
    let list = parts.next().unwrap_or_default();
    let mut fonts = Vec::new();
    for item in split_top(&list, ',') {
        if let Some(font) = parse_font(&item)? {
            fonts.push(font);
        }
    }
    let mut size = None;
    for flag in parts {
        if let Some(h) = flag.trim().strip_prefix('h') {
            let pt = h
                .trim()
                .parse::<f32>()
                .ok()
                .filter(|pt| pt.is_finite() && *pt > 0.0)
                .ok_or_else(|| format!("bad size :{}", flag.trim()))?;
            size = Some(pt);
        }
    }
    Ok(GuiFont { fonts, size })
}

/// Split `s` at each `sep` outside brackets.
fn split_top(s: &str, sep: char) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut depth = 0usize;
    for c in s.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if c == sep && depth == 0 {
            out.push(String::new());
        } else {
            out.last_mut().expect("never empty").push(c);
        }
    }
    out
}

/// One list item: `Family` or `Family[options]`. `None` for an empty item.
fn parse_font(item: &str) -> Result<Option<FontSpec>, String> {
    let item = item.replace("\\ ", " ");
    let (family, opts) = match item.find('[') {
        Some(open) => {
            let rest = &item[open + 1..];
            let close = rest
                .rfind(']')
                .ok_or_else(|| format!("unclosed [ after {}", item[..open].trim()))?;
            if !rest[close + 1..].trim().is_empty() {
                return Err(format!("text after ] in {}", item.trim()));
            }
            (
                item[..open].trim().to_string(),
                Some(rest[..close].to_string()),
            )
        }
        None => (item.trim().to_string(), None),
    };
    if family.is_empty() {
        return match opts {
            Some(_) => Err("options without a font name".to_string()),
            None => Ok(None),
        };
    }
    let mut font = FontSpec {
        family,
        ..FontSpec::default()
    };
    for opt in opts.iter().flat_map(|o| o.split(';')) {
        let opt = opt.trim();
        if opt.is_empty() {
            continue;
        }
        let (key, val) = opt
            .split_once('=')
            .map(|(k, v)| (k.trim(), v.trim()))
            .ok_or_else(|| format!("{opt} needs a value, on {}", font.family))?;
        match key {
            "weight" => font.weight = Some(parse_weight(val, &font.family)?),
            "scale" => {
                let s = val
                    .parse::<f32>()
                    .ok()
                    .filter(|s| s.is_finite() && *s > 0.0 && *s <= 4.0)
                    .ok_or_else(|| format!("scale={val} on {} (want 0–4)", font.family))?;
                font.scale = Some(s);
            }
            "features" => {
                for f in val.split_whitespace() {
                    font.features.push(parse_feature(f, &font.family)?);
                }
            }
            _ => return Err(format!("unknown option {key} on {}", font.family)),
        }
    }
    Ok(Some(font))
}

fn parse_weight(val: &str, family: &str) -> Result<u16, String> {
    let named = match val.to_ascii_lowercase().as_str() {
        "thin" => Some(100),
        "extralight" => Some(200),
        "light" => Some(300),
        "normal" | "regular" => Some(400),
        "medium" => Some(500),
        "semibold" => Some(600),
        "bold" => Some(700),
        "extrabold" => Some(800),
        "black" => Some(900),
        _ => None,
    };
    named
        .or_else(|| val.parse::<u16>().ok().filter(|w| (1..=1000).contains(w)))
        .ok_or_else(|| format!("weight={val} on {family} (want 1–1000 or a name)"))
}

fn parse_feature(f: &str, family: &str) -> Result<([u8; 4], u16), String> {
    let (tag, value) = if let Some(t) = f.strip_prefix('+') {
        (t, 1)
    } else if let Some(t) = f.strip_prefix('-') {
        (t, 0)
    } else if let Some((t, n)) = f.split_once('=') {
        let n = n
            .parse::<u16>()
            .map_err(|_| format!("feature {f} on {family}: the value isn't a number"))?;
        (t, n)
    } else {
        (f, 1)
    };
    let bytes = tag.as_bytes();
    if bytes.len() != 4 || !bytes.iter().all(|b| b.is_ascii_graphic()) {
        return Err(format!(
            "feature {f} on {family}: a tag is four letters or digits"
        ));
    }
    Ok(([bytes[0], bytes[1], bytes[2], bytes[3]], value))
}
