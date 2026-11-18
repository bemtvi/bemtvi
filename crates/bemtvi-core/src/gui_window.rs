//! The native GUI's window options — `'guimaximize'`, `'guiposition'`, `'guisize'` — as
//! modes: what each accepts, spelled one way. The core validates every write against
//! these (`:set`, `btv.o`, `btv.wso`); the GUI reads the relayed values with the same
//! parsers, so the two can't drift apart.

/// Whether the window opens maximized: `'guimaximize'`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowMaximize {
    /// As it was when the GUI last closed. The default.
    #[default]
    Remember,
    /// Always maximized: maximize it now, and open it maximized next time.
    Always,
    /// Never maximized: restore it now, and open it restored next time.
    Never,
}

impl WindowMaximize {
    /// The mode `spec` spells (lower case), or `None`.
    pub fn parse(spec: &str) -> Option<Self> {
        match spec {
            "remember" => Some(Self::Remember),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// The option's spelling of it.
    pub fn spec(self) -> &'static str {
        match self {
            Self::Remember => "remember",
            Self::Always => "always",
            Self::Never => "never",
        }
    }
}

/// Where the window opens: `'guiposition'`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowPosition {
    /// Where it was when the GUI last closed. The default.
    #[default]
    Remember,
    /// Centred on its screen: centre it now, and open it centred from now on.
    Centered,
}

impl WindowPosition {
    /// The mode `spec` spells (lower case), or `None`.
    pub fn parse(spec: &str) -> Option<Self> {
        match spec {
            "remember" => Some(Self::Remember),
            "centered" => Some(Self::Centered),
            _ => None,
        }
    }

    /// The option's spelling of it.
    pub fn spec(self) -> &'static str {
        match self {
            Self::Remember => "remember",
            Self::Centered => "centered",
        }
    }
}

/// How large the window opens: `'guisize'`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum WindowSizing {
    /// As it was when the GUI last closed. The default.
    #[default]
    Remember,
    /// This share (`0.01`–`1.0` each side) of its screen's work area: size it so now,
    /// and open it so from now on. Spelled `{W}%x{H}%`, each a whole percentage from 1
    /// to 100.
    Screen { width: f32, height: f32 },
}

impl WindowSizing {
    /// The mode `spec` spells (lower case): `remember` or `{W}%x{H}%`. `None` for
    /// anything else, a share out of range included.
    pub fn parse(spec: &str) -> Option<Self> {
        if spec == "remember" {
            return Some(Self::Remember);
        }
        let (width, height) = spec.split_once('x')?;
        let percent = |side: &str| {
            let n: u8 = side.trim().strip_suffix('%')?.trim().parse().ok()?;
            (1..=100).contains(&n).then_some(f32::from(n) / 100.0)
        };
        Some(Self::Screen {
            width: percent(width)?,
            height: percent(height)?,
        })
    }

    /// The option's spelling of it.
    pub fn spec(self) -> String {
        match self {
            Self::Remember => "remember".to_owned(),
            Self::Screen { width, height } => {
                format!("{}%x{}%", (width * 100.0).round(), (height * 100.0).round())
            }
        }
    }
}

/// Whether `name` is one of the window options.
pub fn is_window_option(name: &str) -> bool {
    matches!(name, "guimaximize" | "guiposition" | "guisize")
}

/// Checks a value written to one of the window options, whatever the path (`:set`,
/// `btv.o`, a workspace override): `None` if `name` isn't one of them; else its
/// canonical spelling, or the `E474` that refuses it.
pub fn check(name: &str, value: &str) -> Option<Result<String, String>> {
    let canon = value.trim().to_ascii_lowercase();
    let spec = match name {
        "guimaximize" => WindowMaximize::parse(&canon).map(|m| m.spec().to_owned()),
        "guiposition" => WindowPosition::parse(&canon).map(|m| m.spec().to_owned()),
        "guisize" => WindowSizing::parse(&canon).map(WindowSizing::spec),
        _ => return None,
    };
    Some(spec.ok_or_else(|| format!("E474: Invalid argument: {name}={value}")))
}
