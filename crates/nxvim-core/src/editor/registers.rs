//! The register file — vim's named, numbered, and special registers.
//!
//! Replaces the single unnamed slot with the full set: the unnamed `"`, the
//! yank register `"0`, the delete ring `"1`–`"9`, the small-delete `"-`, the
//! named `"a`–`"z` (uppercase appends), and the black-hole `"_`. Every *routing*
//! rule — which yank/delete lands in which register — lives here, so the
//! operators that call in stay oblivious to register bookkeeping. Pure: no I/O,
//! no Lua. (Read-only specials `"%` `".` `":` `"/`, the clipboard `"+` `"*`, and
//! `"=` arrive in later phases.)

use std::collections::HashMap;

/// Whether a register's contents paste back charwise or linewise. (Blockwise
/// waits on visual-block mode.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum RegKind {
    #[default]
    Char,
    Line,
}

/// One register's stored text plus how it pastes.
#[derive(Debug, Clone, Default)]
pub(crate) struct RegisterCell {
    pub(crate) text: String,
    pub(crate) kind: RegKind,
}

/// The whole register file, keyed by register name: the unnamed register is the
/// `'"'` key, numbered registers are `'0'..='9'`, small-delete is `'-'`, and
/// named registers are `'a'..='z'`. The black hole `'_'` is never stored.
#[derive(Debug, Clone, Default)]
pub(crate) struct Registers {
    cells: HashMap<char, RegisterCell>,
}

impl Registers {
    /// Record a yank. An explicit `"x` register is written directly (uppercase
    /// appends) and mirrored into the unnamed register. With no explicit
    /// register, the yank fills the unnamed register **and** the yank register
    /// `"0` — vim's rule that a yank never disturbs the `"1`–`"9` delete ring.
    pub(crate) fn record_yank(&mut self, reg: Option<char>, text: String, kind: RegKind) {
        match reg {
            Some('_') => {}
            Some(name) => self.write_named(name, text, kind),
            None => {
                self.set('0', text.clone(), kind);
                self.set('"', text, kind);
            }
        }
    }

    /// Record a delete (also a change `c`). An explicit register behaves like a
    /// yank. With no explicit register the unnamed register is filled, then the
    /// text is routed: a linewise or multi-line delete shifts the `"1`–`"9`
    /// ring; a small (within-one-line) delete lands in the small-delete `"-`.
    pub(crate) fn record_delete(&mut self, reg: Option<char>, text: String, kind: RegKind) {
        match reg {
            Some('_') => {}
            Some(name) => self.write_named(name, text, kind),
            None => {
                if kind == RegKind::Line || text.contains('\n') {
                    self.shift_ring(text.clone(), kind);
                } else {
                    self.set('-', text.clone(), kind);
                }
                self.set('"', text, kind);
            }
        }
    }

    /// Read a register for paste. `None` ⇒ the unnamed register; an uppercase
    /// name reads the underlying lowercase register. Returns `None` for an empty
    /// / never-written register (paste then does nothing).
    pub(crate) fn get(&self, reg: Option<char>) -> Option<&RegisterCell> {
        let name = reg.unwrap_or('"').to_ascii_lowercase();
        self.cells.get(&name)
    }

    /// Exact lookup of a stored register by name (no unnamed fallback, no
    /// upper→lower folding) — for the `:registers` listing.
    pub(crate) fn peek(&self, name: char) -> Option<&RegisterCell> {
        self.cells.get(&name)
    }

    /// Every stored register as `(name, text, kind)`, for the Rust→Lua mirror
    /// `vim.fn.getreg` / `getregtype` read against. Order is unspecified (the
    /// Lua side keys by name); read-only specials are added by the caller.
    pub(crate) fn entries(&self) -> Vec<(char, &str, RegKind)> {
        self.cells
            .iter()
            .map(|(name, cell)| (*name, cell.text.as_str(), cell.kind))
            .collect()
    }

    /// Write a register from the `setreg()` API. Unlike a yank/delete this does
    /// **not** mirror into the unnamed register — vim's `setreg` touches only the
    /// named target (the unnamed register follows only via the explicit `u`
    /// option, which Phase 4 does not wire). `append` concatenates to the
    /// existing contents, staying linewise if either part is; the black hole
    /// `'_'` discards. The name is folded to lowercase (uppercase append is
    /// resolved by the caller into `append = true`).
    pub(crate) fn set_api(&mut self, name: char, text: String, kind: RegKind, append: bool) {
        if name == '_' {
            return;
        }
        let name = name.to_ascii_lowercase();
        if append {
            let (mut buf, was_line) = match self.cells.get(&name) {
                Some(cell) => (cell.text.clone(), cell.kind == RegKind::Line),
                None => (String::new(), false),
            };
            buf.push_str(&text);
            let merged = if was_line || kind == RegKind::Line {
                RegKind::Line
            } else {
                kind
            };
            self.set(name, buf, merged);
        } else {
            self.set(name, text, kind);
        }
    }

    /// Write an explicitly named register: uppercase `A`–`Z` *appends* to the
    /// lowercase register (keeping it linewise if either part is), lowercase /
    /// digit / `-` overwrites. The unnamed register mirrors the result.
    fn write_named(&mut self, name: char, text: String, kind: RegKind) {
        if name.is_ascii_uppercase() {
            let lower = name.to_ascii_lowercase();
            let (mut buf, was_line) = match self.cells.get(&lower) {
                Some(cell) => (cell.text.clone(), cell.kind == RegKind::Line),
                None => (String::new(), false),
            };
            buf.push_str(&text);
            let merged = if was_line || kind == RegKind::Line {
                RegKind::Line
            } else {
                kind
            };
            self.set(lower, buf.clone(), merged);
            self.set('"', buf, merged);
        } else {
            self.set(name, text.clone(), kind);
            self.set('"', text, kind);
        }
    }

    fn set(&mut self, name: char, text: String, kind: RegKind) {
        self.cells.insert(name, RegisterCell { text, kind });
    }

    /// Shift the numbered delete ring up by one — `"1`→`"2`→…→`"9` (the old `"9`
    /// falls off the end) — then drop the freshest delete into `"1`.
    fn shift_ring(&mut self, text: String, kind: RegKind) {
        for n in (1..9).rev() {
            let from = char::from_digit(n, 10).unwrap();
            let to = char::from_digit(n + 1, 10).unwrap();
            if let Some(cell) = self.cells.get(&from).cloned() {
                self.cells.insert(to, cell);
            }
        }
        self.set('1', text, kind);
    }
}
