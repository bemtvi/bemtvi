//! Path syntax of a filesystem this process does **not** run on — a daemon's.
//!
//! In a daemon (`--connect-daemon`) session the edit-host runs on the client while the
//! files live on the daemon's host, so every path that crosses the wire is spelled in the
//! *daemon's* syntax. `std::path` can't be used to manipulate those strings: it applies
//! the *client's* rules, so a Windows client sees the unix path `/virtual/x` as relative
//! (no drive) and joins `src` onto `/virtual/proj` with a `\`. The daemon announces which
//! syntax its filesystem uses (the `config_bundle` handshake's `path_style` field), and
//! every remote path operation — absolute?, join, normalize, parent, file name, `file://`
//! URI — goes through [`PathStyle`] parameterized by that announcement, never by sniffing
//! a string's shape. Remote paths stay plain `String`s in the daemon's syntax end to end.
//!
//! A local session never uses this: it keeps `std::path`, whose rules *are* the host's.

/// The path syntax of a filesystem: POSIX (`/` separator, `/`-rooted absolute paths) or
/// Windows (`\` separator, `/` accepted too, drive- or UNC-prefixed absolute paths).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathStyle {
    /// Unix-like: `/a/b`.
    Posix,
    /// Windows: `C:\a\b`, `\\server\share\a`.
    Windows,
}

impl PathStyle {
    /// The syntax of the filesystem this process runs on — what a daemon announces about
    /// itself, and the fallback when an older daemon announced nothing (exactly the rules
    /// the edit-host applied before the field existed).
    pub const fn native() -> Self {
        if cfg!(windows) {
            PathStyle::Windows
        } else {
            PathStyle::Posix
        }
    }

    /// The wire spelling (`"posix"` / `"windows"`), the inverse of [`from_wire`](Self::from_wire).
    pub const fn wire_name(self) -> &'static str {
        match self {
            PathStyle::Posix => "posix",
            PathStyle::Windows => "windows",
        }
    }

    /// Parse a wire spelling; `None` for anything unrecognized.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "posix" => Some(PathStyle::Posix),
            "windows" => Some(PathStyle::Windows),
            _ => None,
        }
    }

    /// The preferred separator.
    pub const fn sep(self) -> char {
        match self {
            PathStyle::Posix => '/',
            PathStyle::Windows => '\\',
        }
    }

    /// Whether `c` separates components.
    pub fn is_sep(self, c: char) -> bool {
        match self {
            PathStyle::Posix => c == '/',
            PathStyle::Windows => c == '/' || c == '\\',
        }
    }

    /// Split `p` into its prefix + root (`"/"`, `"C:\"`, `"\\server\share\"`, `"C:"`,
    /// `"\"`, or `""` for a relative path) and the rest.
    fn split_root(self, p: &str) -> (&str, &str) {
        match self {
            PathStyle::Posix => match p.strip_prefix('/') {
                Some(rest) => ("/", rest),
                None => ("", p),
            },
            PathStyle::Windows => {
                let b = p.as_bytes();
                let sep = |i: usize| b.get(i).is_some_and(|&c| c == b'/' || c == b'\\');
                // UNC: `\\server\share\rest`.
                if sep(0) && sep(1) {
                    let mut seps = 0;
                    for (i, c) in p.char_indices().skip(2) {
                        if self.is_sep(c) {
                            seps += 1;
                            if seps == 2 {
                                return (&p[..=i], &p[i + 1..]);
                            }
                        }
                    }
                    return (p, "");
                }
                // Drive: `C:` / `C:\`.
                if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
                    return if sep(2) {
                        (&p[..3], &p[3..])
                    } else {
                        (&p[..2], &p[2..])
                    };
                }
                if sep(0) {
                    (&p[..1], &p[1..])
                } else {
                    ("", p)
                }
            }
        }
    }

    /// Whether `p` is absolute in this syntax (Posix: `/`-rooted; Windows: a drive with a
    /// root, or a UNC path — a bare `\x` or `C:x` is not, matching `std::path` there).
    pub fn is_absolute(self, p: &str) -> bool {
        let (root, _) = self.split_root(p);
        match self {
            PathStyle::Posix => !root.is_empty(),
            PathStyle::Windows => {
                root.len() >= 3 || root.starts_with("\\\\") || root.starts_with("//")
            }
        }
    }

    /// `base` joined with `p`: `p` itself when it is absolute, else `p` appended under
    /// `base` with this syntax's separator. A Windows rooted-but-driveless `p` (`\x`)
    /// keeps `base`'s drive, as `std::path` does on Windows.
    pub fn join(self, base: &str, p: &str) -> String {
        if self.is_absolute(p) {
            return p.to_string();
        }
        let (root, _) = self.split_root(p);
        if !root.is_empty() {
            // Windows only: `\x` (rooted, no drive) or `C:x` (drive-relative).
            if root.ends_with(':') {
                return p.to_string();
            }
            let (base_root, _) = self.split_root(base);
            let prefix = base_root.trim_end_matches(|c| self.is_sep(c));
            return format!("{prefix}{p}");
        }
        if base.is_empty() {
            return p.to_string();
        }
        if p.is_empty() {
            return base.to_string();
        }
        let mut out = base.to_string();
        if !base.ends_with(|c| self.is_sep(c)) {
            out.push(self.sep());
        }
        out.push_str(p);
        // A Windows *verbatim* base (`\\?\C:\…`, what `canonicalize` returns there) is
        // never normalized by the OS, so a `.` / `..` appended to it would name nothing;
        // fold them now, as `std::path`'s `push` does for a verbatim path.
        if self == PathStyle::Windows && base.starts_with(r"\\?\") {
            return self.normalize(&out);
        }
        out
    }

    /// Lexically normalize `p` — drop `.` and empty components, fold `..` into its parent
    /// (never climbing above a root), and spell separators with [`sep`](Self::sep). No
    /// filesystem access, so symlinks are not resolved.
    pub fn normalize(self, p: &str) -> String {
        let (root, rest) = self.split_root(p);
        let mut parts: Vec<&str> = Vec::new();
        for comp in rest.split(|c| self.is_sep(c)) {
            match comp {
                "" | "." => {}
                ".." => {
                    if matches!(parts.last(), Some(&last) if last != "..") {
                        parts.pop();
                    } else if root.is_empty() {
                        parts.push("..");
                    }
                }
                c => parts.push(c),
            }
        }
        let mut out: String = match self {
            PathStyle::Posix => root.to_string(),
            PathStyle::Windows => root.replace('/', "\\"),
        };
        let sep = self.sep().to_string();
        out.push_str(&parts.join(&sep));
        if out.is_empty() {
            out.push('.');
        }
        out
    }

    /// The parent of `p` (the path minus its last component), or `None` for a bare root
    /// or a single relative component.
    pub fn parent(self, p: &str) -> Option<&str> {
        let (root, rest) = self.split_root(p);
        let rest = rest.trim_end_matches(|c| self.is_sep(c));
        if rest.is_empty() {
            return None;
        }
        match rest.rfind(|c| self.is_sep(c)) {
            Some(i) => {
                let head = rest[..i].trim_end_matches(|c| self.is_sep(c));
                Some(&p[..root.len() + head.len()])
            }
            None if root.is_empty() => None,
            None => Some(root),
        }
    }

    /// The last component of `p`, or `None` for a bare root / empty path.
    pub fn file_name(self, p: &str) -> Option<&str> {
        let (_, rest) = self.split_root(p);
        let rest = rest.trim_end_matches(|c| self.is_sep(c));
        let name = match rest.rfind(|c| self.is_sep(c)) {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        (!name.is_empty() && name != "." && name != "..").then_some(name)
    }

    /// The `file://` URI naming the absolute path `p` (`None` for a relative one):
    /// `file:///a/b` for POSIX, `file:///C:/a/b` / `file://server/share/a` for Windows,
    /// with each component percent-encoded.
    pub fn to_file_uri(self, p: &str) -> Option<String> {
        if !self.is_absolute(p) {
            return None;
        }
        let mut out = String::from("file://");
        let (host, path) = match self {
            PathStyle::Posix => ("", p.to_string()),
            PathStyle::Windows => {
                let (root, rest) = self.split_root(p);
                if root.starts_with("\\\\") || root.starts_with("//") {
                    // `\\server\share\` → host `server`, path `/share/rest`.
                    let inner: Vec<&str> = root[2..]
                        .split(|c| self.is_sep(c))
                        .filter(|s| !s.is_empty())
                        .collect();
                    let host = inner.first().copied().unwrap_or("");
                    let share = inner.get(1).copied().unwrap_or("");
                    (host, format!("/{share}/{}", rest.replace('\\', "/")))
                } else {
                    ("", format!("/{}/{}", &root[..2], rest.replace('\\', "/")))
                }
            }
        };
        out.push_str(host);
        for (i, seg) in path.split('/').enumerate() {
            if i > 0 {
                out.push('/');
            }
            percent_encode_into(seg, &mut out);
        }
        Some(out)
    }

    /// The path a `file://` URI names, spelled in this syntax — the inverse of
    /// [`to_file_uri`](Self::to_file_uri). `None` for a non-`file` URI or an
    /// undecodable one.
    pub fn from_file_uri(self, uri: &str) -> Option<String> {
        let rest = uri.strip_prefix("file://")?;
        let (host, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        // Drop a query / fragment.
        let path = path.split(['?', '#']).next().unwrap_or(path);
        let path = percent_decode(path)?;
        match self {
            PathStyle::Posix => {
                // A host other than `localhost` names a remote machine POSIX can't spell.
                if !host.is_empty() && host != "localhost" {
                    return None;
                }
                Some(path)
            }
            PathStyle::Windows => {
                if !host.is_empty() && host != "localhost" {
                    return Some(format!("\\\\{host}{}", path.replace('/', "\\")));
                }
                let b = path.as_bytes();
                // `/C:/x` (or the `|` legacy drive spelling).
                if b.len() >= 3
                    && b[0] == b'/'
                    && b[1].is_ascii_alphabetic()
                    && matches!(b[2], b':' | b'|')
                {
                    let rest = path[3..].replace('/', "\\");
                    let rest = if rest.is_empty() {
                        "\\".to_string()
                    } else {
                        rest
                    };
                    return Some(format!("{}:{rest}", &path[1..2]));
                }
                None
            }
        }
    }
}

/// Percent-encode a URI path segment: unreserved characters and the sub-delims a path
/// keeps literal (`Url::from_file_path`'s set) pass through; everything else is `%XX`.
fn percent_encode_into(seg: &str, out: &mut String) {
    for &b in seg.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
                    | b':'
                    | b'@'
            );
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
}

/// Decode `%XX` escapes; `None` when the result is not UTF-8.
fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}
