//! The server-side key-mapping engine: a per-mode prefix trie plus an N-key
//! withhold/replay buffer that sits *in front of* `Editor::input`, so the core
//! key state machine never learns about user mappings (design D1).
//!
//! The whole engine is a pure function of *(mode, incoming key)*: [`Keymaps::feed`]
//! returns an owned list of [`Step`]s the server then executes against the editor
//! / Lua — it never touches the editor itself, which both keeps the core mapping-
//! unaware and sidesteps borrow conflicts between the matcher state and the editor.
//!
//! **Matching without a timer.** nxvim processes keys synchronously, in
//! `nvim_input` batches, with no idle timer — so vim's `timeoutlen` ambiguity
//! ("wait T ms, then take the shorter map") can't be reproduced. Instead a key
//! that forms a *live prefix* of some mapping is **withheld** in `pending`; the
//! next key either extends the prefix, completes a mapping, or breaks it — in
//! which case the buffered keys are **replayed** to the editor and the current key
//! re-processed (this is the generalization the LSP branch's hand-rolled
//! `lsp_pending_g` recognizer becomes on the backport). The one divergence from
//! neovim: a trailing live-prefix with no following key stays buffered until the
//! next keystroke flushes it, rather than resolving on a wall-clock timeout.

use std::collections::HashMap;

use nxvim_core::{parse_keys, Key, Mode};
use nxvim_lua::{RawKeymap, RawRhs};

/// What a matched mapping does when it fires (design D7). The fire dispatch is a
/// `match` over this enum from day one, so the LSP backport adds its native
/// action as one more variant + one more arm — not an engine change.
#[derive(Clone, Debug)]
pub enum MappingRhs {
    /// A Lua function RHS, keyed by id in `vim._keymap_fns`; the server runs it
    /// via `LuaRuntime::run_keymap` and folds in the effects.
    Lua(u64),
    /// A string RHS already parsed to keys, with its `noremap` flag. Phase 1
    /// only feeds `noremap` RHSs straight to the editor; recursive (remap)
    /// feeding through the matcher is Phase 2.
    Keys(Vec<Key>, bool),
}

/// A unit of work [`Keymaps::feed`] hands back for the server to apply, in order.
pub enum Step {
    /// Send this key to `editor.input` (then `emit_lifecycle_events`).
    Editor(Key),
    /// Fire this mapping's RHS.
    Fire(MappingRhs),
}

/// A node in a per-mode prefix trie: the mapping that ends here (if any) and the
/// continuations that extend it.
#[derive(Default)]
struct Node {
    children: HashMap<Key, Node>,
    mapping: Option<MappingRhs>,
}

/// A prefix trie of LHS key-paths → mapping, one per mode.
#[derive(Default)]
struct Trie {
    root: Node,
}

/// How a buffered key sequence relates to the mappings in a trie.
enum Classify {
    /// A live prefix of at least one mapping (possibly itself complete): hold.
    Prefix,
    /// A complete mapping that no longer mapping extends: fire it now.
    Complete(MappingRhs),
    /// Not a prefix of anything: the sequence broke every live mapping.
    None,
}

impl Trie {
    /// Insert `keys` → `rhs`. A later insert at the same path overwrites an
    /// earlier one, which is how the precedence ladder resolves to last-wins
    /// (callers insert lowest-precedence first; see [`Keymaps::rebuild`]).
    fn insert(&mut self, keys: &[Key], rhs: MappingRhs) {
        let mut node = &mut self.root;
        for k in keys {
            node = node.children.entry(*k).or_default();
        }
        node.mapping = Some(rhs);
    }

    /// Classify `keys` against the trie. `Complete` is returned only for a
    /// mapping with no longer continuation — a mapping that is *also* a prefix of
    /// a longer one (ambiguous, e.g. `j` & `jk`) is held as `Prefix` and resolved
    /// when a later key breaks it (via [`Trie::longest_complete`]).
    fn classify(&self, keys: &[Key]) -> Classify {
        let mut node = &self.root;
        for k in keys {
            match node.children.get(k) {
                Some(n) => node = n,
                None => return Classify::None,
            }
        }
        match (&node.mapping, node.children.is_empty()) {
            (Some(rhs), true) => Classify::Complete(rhs.clone()),
            (Some(_), false) => Classify::Prefix, // complete but also a prefix: hold
            (None, false) => Classify::Prefix,    // live prefix, not yet complete
            (None, true) => Classify::None,       // unreachable in a well-formed trie
        }
    }

    /// The longest prefix of `keys` that is a complete mapping, with its length —
    /// used to fire the shorter map when an ambiguous sequence finally breaks.
    fn longest_complete(&self, keys: &[Key]) -> Option<(MappingRhs, usize)> {
        let mut node = &self.root;
        let mut best = None;
        for (i, k) in keys.iter().enumerate() {
            match node.children.get(k) {
                Some(n) => {
                    node = n;
                    if let Some(rhs) = &node.mapping {
                        best = Some((rhs.clone(), i + 1));
                    }
                }
                None => break,
            }
        }
        best
    }
}

/// The mapping engine: cached per-mode tries, the registry version they were
/// built from, and the withhold/replay buffer. One of these lives on the server.
#[derive(Default)]
pub struct Keymaps {
    /// `vim._keymaps_version` the tries were last compiled from. The server
    /// rebuilds only when the live version advances (checked once per batch).
    pub version: u64,
    /// Per-mode tries, keyed by mode code (`'n'`, `'i'`, …).
    tries: HashMap<char, Trie>,
    /// Keys withheld as a live prefix, awaiting the key that extends, completes,
    /// or breaks them. Persists across batches (no auto-flush — design D4).
    pending: Vec<Key>,
}

impl Keymaps {
    /// (Re)compile the per-mode tries from a registry snapshot. Entries are
    /// applied in precedence order — **buffer-local > global**, within a scope
    /// **user (non-default) > default**, and among equals **last-set wins** — by
    /// inserting lowest-precedence first so higher-precedence entries overwrite at
    /// the same LHS path (D6). Phase 1 exercises only the last-set-wins rung
    /// (all maps are global, non-default); the buffer/default keys are present so
    /// later phases and the LSP backport are a data change, not an engine change.
    pub fn rebuild(&mut self, version: u64, mut snapshot: Vec<RawKeymap>) {
        self.version = version;
        self.tries.clear();
        snapshot.sort_by_key(|e| (e.buffer.is_some(), !e.default, e.seq));
        for entry in snapshot {
            let lhs = parse_keys(&entry.lhs);
            if lhs.is_empty() {
                continue;
            }
            let rhs = match entry.rhs {
                RawRhs::Lua(id) => MappingRhs::Lua(id),
                RawRhs::Str(s) => MappingRhs::Keys(parse_keys(&s), entry.noremap),
            };
            for mode in &entry.modes {
                // Phase 1 stores maps under their raw single-char mode code; the
                // mode-list fan-out and the v/x/o equivalences are Phase 2.
                if let Some(code) = mode.chars().next() {
                    self.tries
                        .entry(code)
                        .or_default()
                        .insert(&lhs, rhs.clone());
                }
            }
        }
    }

    /// Feed one input key in `mode` and return the steps it produced. The server
    /// calls this for every parsed key, executing the steps in order.
    pub fn feed(&mut self, mode: Mode, key: Key) -> Vec<Step> {
        let mut steps = Vec::new();
        self.feed_key(mode, key, &mut steps);
        steps
    }

    fn feed_key(&mut self, mode: Mode, key: Key, steps: &mut Vec<Step>) {
        let mode_key = mode_key(mode);
        // No mappings for this mode: flush any prefix buffered in another mode and
        // pass the key straight through. (In practice `pending` is empty here.)
        if !self.tries.contains_key(&mode_key) {
            steps.extend(self.pending.drain(..).map(Step::Editor));
            steps.push(Step::Editor(key));
            return;
        }
        self.pending.push(key);
        let classify = self.tries[&mode_key].classify(&self.pending);
        match classify {
            Classify::Prefix => {} // hold: wait for the next key
            Classify::Complete(rhs) => {
                self.pending.clear();
                steps.push(Step::Fire(rhs));
            }
            Classify::None => {
                // This key broke every live prefix. Resolve the previously
                // buffered keys (without this key), then re-process this key fresh.
                self.pending.pop();
                let buffered: Vec<Key> = self.pending.drain(..).collect();
                if buffered.is_empty() {
                    // The key on its own starts no mapping: straight to the editor.
                    steps.push(Step::Editor(key));
                } else {
                    self.resolve_buffered(mode_key, &buffered, steps);
                    self.feed_key(mode, key, steps);
                }
            }
        }
    }

    /// Resolve a run of buffered keys that no longer extends any mapping: fire the
    /// longest complete mapping that prefixes them (the ambiguous shorter map),
    /// then replay the remainder to the editor; with no complete prefix, replay
    /// them all (the withheld keys were not a mapping). Phase 1 replays the
    /// remainder raw — re-feeding it through the matcher is part of remap (Phase 2).
    fn resolve_buffered(&mut self, mode_key: char, buffered: &[Key], steps: &mut Vec<Step>) {
        match self.tries[&mode_key].longest_complete(buffered) {
            Some((rhs, used)) => {
                steps.push(Step::Fire(rhs));
                steps.extend(buffered[used..].iter().copied().map(Step::Editor));
            }
            None => steps.extend(buffered.iter().copied().map(Step::Editor)),
        }
    }
}

/// The trie key for an editor mode — the first char of its `mode()` short code
/// (`Normal` → `'n'`, `Insert` → `'i'`, …).
fn mode_key(mode: Mode) -> char {
    mode.short_code().chars().next().unwrap_or('n')
}
