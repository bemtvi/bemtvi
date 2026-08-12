//! The `gc`/`gcc` comment operator and the `'commentstring'` machinery it reads.
//!
//! A faithful port of neovim's built-in `vim._comment` (runtime/lua/vim/
//! `_comment.lua`): the per-line comment/uncomment functions, the "is this block
//! already commented?" decision, and the indent-aligned toggle. The comment
//! template is resolved per buffer by [`Editor::effective_commentstring`] — an
//! explicit `:set commentstring` / `btv.bo.commentstring` override, else the
//! filetype's built-in default ([`commentstring_for_language`]).

use super::*;

/// The two halves of a `'commentstring'` (`<left>%s<right>`), e.g. `("// ", "")`
/// for a line comment or `("/* ", " */")` for a block one. Split on the literal
/// `%s`, the surrounding spaces kept as written.
struct Parts {
    left: String,
    right: String,
}

impl Parts {
    /// Split a `commentstring` on its `%s` placeholder. `None` when the template
    /// has no `%s` (an invalid `commentstring`, which `gc` then refuses).
    fn parse(cs: &str) -> Option<Parts> {
        let idx = cs.find("%s")?;
        Some(Parts {
            left: cs[..idx].to_string(),
            right: cs[idx + 2..].to_string(),
        })
    }
}

/// Whether `line` already looks commented under `parts`: optional indent, then the
/// *trimmed* left part, anything, the *trimmed* right part, then optional trailing
/// whitespace — neovim's `make_comment_check` regex without the regex.
fn is_commented(line: &str, parts: &Parts) -> bool {
    let lt = parts.left.trim();
    let rt = parts.right.trim();
    let body = line.trim_start();
    let Some(after_left) = body.strip_prefix(lt) else {
        return false;
    };
    after_left.trim_end().ends_with(rt)
}

/// Whether `line` is blank (empty or all whitespace).
fn is_blank(line: &str) -> bool {
    line.chars().all(char::is_whitespace)
}

/// The leading-whitespace prefix of `line` (the actual bytes — tabs and spaces
/// kept verbatim, as neovim copies rather than rebuilds the indent).
fn indent_of(line: &str) -> &str {
    let end = line
        .find(|c: char| !c.is_whitespace())
        .unwrap_or(line.len());
    &line[..end]
}

/// Comment one line: a blank line becomes `indent + trim(left) + trim(right)`
/// (no trailing space); a content line becomes `block_indent + left + <rest> +
/// right`, where the block's common `indent` replaces each line's own up to that
/// width. Mirrors neovim's `make_comment_function`.
fn comment_line(line: &str, parts: &Parts, indent: &str) -> String {
    if is_blank(line) {
        return format!("{}{}{}", indent, parts.left.trim(), parts.right.trim());
    }
    // `indent` is the block's minimum indent, never longer than this (non-blank)
    // line's own — so slicing off `indent.len()` bytes always lands on the line's
    // remaining content.
    let rest = &line[indent.len()..];
    format!("{}{}{}{}", indent, parts.left, rest, parts.right)
}

/// Uncomment one line: strip the left/right parts (exact first, then trimmed) and
/// return the inside, preserving indent and trailing whitespace. Returns the line
/// unchanged when it is not actually commented. Mirrors neovim's
/// `make_uncomment_function`.
fn uncomment_line(line: &str, parts: &Parts) -> String {
    let strip = |l: &str, r: &str| -> Option<(String, String, String)> {
        let indent = indent_of(line);
        let rest = &line[indent.len()..];
        let inner = rest.strip_prefix(l)?;
        let trimmed = inner.trim_end();
        let trail = &inner[trimmed.len()..];
        let core = trimmed.strip_suffix(r)?;
        Some((indent.to_string(), core.to_string(), trail.to_string()))
    };
    // Exact parts first, then the trimmed fallback (a `//foo` line still
    // uncomments under a `"// "` left part).
    let Some((mut indent, core, mut trail)) =
        strip(&parts.left, &parts.right).or_else(|| strip(parts.left.trim(), parts.right.trim()))
    else {
        return line.to_string();
    };
    // A now-blank inner drops indent/trail so the line ends up genuinely empty
    // rather than carrying stray whitespace.
    if is_blank(&core) {
        indent.clear();
        trail.clear();
    }
    format!("{indent}{core}{trail}")
}

/// Map a **filetype** to its default `'commentstring'`, the built-in template
/// `gc` uses when a buffer has no explicit override. Covers the most popular
/// languages so commenting works out of the box; an unknown filetype returns
/// `None` and `gc` then reports the empty-`commentstring` warning.
///
/// The keys are filetype names, not extensions: a buffer reaches one either via
/// [`language_of_path`] (its extension) or an explicit `btv.bo.filetype` / `:setf`,
/// so this set is intentionally broader than the extension table — it also covers
/// languages whose filetype is usually set by config rather than guessed from a
/// suffix (e.g. `ruby`, `java`, `make`).
pub fn commentstring_for_language(lang: &str) -> Option<&'static str> {
    Some(match lang {
        // C-family and friends: line comments.
        "rust" | "c" | "cpp" | "java" | "javascript" | "typescript" | "tsx" | "go" | "kotlin"
        | "swift" | "scala" | "dart" | "php" | "zig" => "// %s",
        // Hash-comment scripting / config languages.
        "python" | "ruby" | "shell" | "bash" | "perl" | "yaml" | "toml" | "make" | "dockerfile"
        | "r" | "elixir" => "# %s",
        // Lua and SQL: double dash.
        "lua" | "sql" | "haskell" => "-- %s",
        // Lisps.
        "lisp" | "clojure" | "scheme" => "; %s",
        // Markup and styles: block comments only.
        "html" | "xml" | "markdown" => "<!-- %s -->",
        "css" | "scss" => "/* %s */",
        // VimL.
        "vim" => "\" %s",
        _ => return None,
    })
}

impl Editor {
    /// The buffer's explicit `'commentstring'` override, or `None` when it has none
    /// (so reads fall through to the filetype default). Empty stored strings count
    /// as "no override".
    pub(crate) fn commentstring_override(&self, buf: BufferId) -> Option<&str> {
        self.commentstrings
            .get(&buf)
            .map(String::as_str)
            .filter(|s| !s.is_empty())
    }

    /// Set (or, with an empty string, clear) the **global value** of `'commentstring'` —
    /// the fallback a buffer with no override of its own resolves through. The
    /// `:setglobal` / `vim.go` half of [`Editor::set_commentstring`].
    pub(crate) fn set_commentstring_global(&mut self, value: &str) {
        self.commentstring_global = value.to_string();
    }

    /// The global value of `'commentstring'` (empty ⇒ none), for the `:setglobal cms?`
    /// readout and the `vim.go` mirror.
    pub fn commentstring_global(&self) -> &str {
        &self.commentstring_global
    }

    /// Set `buf`'s explicit `'commentstring'`. An empty string clears the override
    /// (the buffer falls back to the global value, then its filetype default), matching
    /// how `:set cms=` reads in vim — an empty template is "use the default", not "no
    /// comments".
    pub(crate) fn set_commentstring(&mut self, buf: BufferId, value: &str) {
        if value.is_empty() {
            self.commentstrings.remove(&buf);
        } else {
            self.commentstrings.insert(buf, value.to_string());
        }
    }

    /// The `'commentstring'` `gc` actually uses for `buf`: the explicit override if
    /// set, else the global value (`:setglobal cms=…`), else the filetype's built-in
    /// default ([`commentstring_for_language`]), else empty. Backs the comment
    /// operator, the `:set commentstring?` echo, and the `btv.bo.commentstring` mirror.
    pub fn effective_commentstring(&self, buf: BufferId) -> String {
        if let Some(cs) = self.commentstring_override(buf) {
            return cs.to_string();
        }
        // The buffer set none of its own: fall back to the global value (`:setglobal
        // commentstring=…`) before the filetype's built-in template, so a config can
        // override the default for every buffer without touching each one.
        if !self.commentstring_global.is_empty() {
            return self.commentstring_global.clone();
        }
        self.buffer_filetype(buf)
            .and_then(|ft| commentstring_for_language(&ft))
            .unwrap_or("")
            .to_string()
    }

    /// Toggle line comments over buffer lines `[first, last]` (inclusive) — the
    /// body of every `gc`/`gcc`/visual-`gc` form, always linewise. If **every**
    /// non-blank line in the range is already commented the range is uncommented;
    /// otherwise the whole range is commented, aligned to the block's minimum
    /// indent (neovim's `toggle_lines`). One undo step covers the whole range; the
    /// cursor settles on the first line's first non-blank.
    pub(crate) fn toggle_comment_lines(&mut self, first: usize, last: usize) {
        let buf = self.cur_buffer();
        let cs = self.effective_commentstring(buf);
        let Some(parts) = Parts::parse(&cs) else {
            // No usable template (empty / no `%s`) — warn rather than silently
            // mangling the buffer, as neovim does.
            self.echo("Option 'commentstring' is empty or invalid");
            return;
        };
        let last = last.min(self.last_line());
        if first > last {
            return;
        }

        // Gather the range and decide direction: commented iff every non-blank
        // line is commented, with the indent = the block's narrowest (over
        // non-blank lines), copied verbatim to keep tabs/spaces intact.
        let lines: Vec<String> = (first..=last).map(|l| self.buffer().line(l)).collect();
        let mut all_commented = true;
        let mut indent = "";
        let mut indent_width = usize::MAX;
        let mut any_content = false;
        for line in &lines {
            if is_blank(line) {
                continue;
            }
            any_content = true;
            let ind = indent_of(line);
            if ind.len() < indent_width {
                indent_width = ind.len();
                indent = ind;
            }
            if all_commented {
                all_commented = is_commented(line, &parts);
            }
        }
        // An all-blank range has nothing to toggle.
        if !any_content {
            return;
        }
        let indent = indent.to_string();

        let new_lines: Vec<String> = lines
            .iter()
            .map(|line| {
                if all_commented {
                    uncomment_line(line, &parts)
                } else {
                    comment_line(line, &parts, &indent)
                }
            })
            .collect();
        if new_lines == lines {
            return;
        }

        self.push_undo();
        let start = self.buffer().line_start(first);
        let end = self.buffer().line_start(last + 1);
        let mut block = new_lines.join("\n");
        block.push('\n');
        self.buffer_mut().remove(start..end);
        self.buffer_mut().insert(start, &block);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.cursor.line = first.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
        self.clamp_cursor();
    }
}
