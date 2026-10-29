//! `btv.fs.gitignore`: one ignore file (`.gitignore`, `.ignore`, `.rgignore`,
//! `.git/info/exclude`) compiled into a matcher, for the `.gitignore`-respecting
//! `btv.fs` walk in `prelude/fs.lua`.
//!
//! The semantics are the `ignore` crate's — ripgrep's own — so negation, anchoring,
//! directory-only patterns and `**` agree with what `rg` decides for the same tree.
//! Only the matching lives here. Reading the files and composing them (deeper files
//! over shallower, `.ignore` over `.gitignore`) is the walk's job, in Lua, because the
//! walk is what runs over the `btv.fs` seam to a daemon or OPFS.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use mlua::{UserData, UserDataMethods, Value};

pub struct LuaGitignore {
    inner: Gitignore,
}

impl LuaGitignore {
    /// Compile `text` (an ignore file's contents). Paths are matched relative to the
    /// file's own directory. A line `git` would reject (an invalid glob) is skipped, as
    /// git and rg skip it; its message is returned beside the matcher so a caller that
    /// wants to report it can.
    pub fn compile(text: &str) -> mlua::Result<(Self, Vec<String>)> {
        let mut builder = GitignoreBuilder::new("");
        let mut errors = Vec::new();
        for line in text.lines() {
            if let Err(e) = builder.add_line(None, line) {
                errors.push(e.to_string());
            }
        }
        let inner = builder
            .build()
            .map_err(|e| mlua::Error::runtime(format!("btv.fs.gitignore: {e}")))?;
        Ok((LuaGitignore { inner }, errors))
    }
}

impl UserData for LuaGitignore {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // `m:match(path, is_dir)` -> `"ignore"` | `"whitelist"` | nil: what this file
        // says about `path` (`/`-separated, relative to the file's directory). The
        // last matching line decides, as in git; nil when no line matches.
        methods.add_method("match", |_, this, (path, is_dir): (Value, Option<bool>)| {
            let path = crate::glob::candidate("btv.fs.gitignore matcher:match", "path", &path)?;
            let path = path.to_string_lossy();
            Ok(
                match this
                    .inner
                    .matched(std::path::Path::new(&*path), is_dir.unwrap_or(false))
                {
                    Match::None => None,
                    Match::Ignore(_) => Some("ignore"),
                    Match::Whitelist(_) => Some("whitelist"),
                },
            )
        });
        // `m:len()` -> how many patterns compiled (blank and comment lines excluded).
        methods.add_method("len", |_, this, ()| {
            Ok(this.inner.num_ignores() + this.inner.num_whitelists())
        });
    }
}
