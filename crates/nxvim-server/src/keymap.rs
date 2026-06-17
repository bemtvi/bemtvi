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
//! re-processed. This generalization subsumes the LSP branch's old hand-rolled
//! `lsp_pending_g` recognizer: a `g`-prefixed map (`gd`/`gD`/`gr` — installed by the
//! LSP plugin on attach, or any user map) is an ordinary entry in the trie, and the
//! [`command_status`] oracle releases a withheld `g`-run the moment it completes a
//! core motion, so `gg` stays whole. The one divergence from
//! neovim: a trailing live-prefix with no following key has no wall-clock
//! `timeoutlen` to resolve it on; instead the client sends a synthetic idle flush
//! ([`Keymaps::flush`], the `nxvim_input_flush` RPC) after `timeoutlen` of no
//! input, which resolves the buffer exactly as the next-key break path would —
//! keeping the server itself timer-free (design D4, Phase 4).
//!
//! **Built-in disambiguation (the colliding-prefix fix).** To avoid lagging a
//! built-in behind a user map that merely shares its prefix (`gg` under a `gh`
//! map), the break path consults the editor's *own* command grammar as a
//! read-only oracle — [`nxvim_core::command_status`], a pure fold over the same
//! `parse_step` the executor runs. When a withheld run has just replayed raw and
//! re-feeding the next key would only re-withhold it, but that run plus the key
//! already forms a *complete* built-in, the key is released to the editor at once
//! (see [`Keymaps::feed_key`]). This keeps D1's spirit — the engine reads the
//! grammar but mutates no editor state — while making every multi-key built-in
//! instant under a colliding prefix. The idle flush above is now reached only for
//! genuinely-ambiguous *mapped* prefixes and lone-prefix release; user maps still
//! win (the oracle fires only where a run breaks every live mapping prefix). Full
//! rationale: `docs/specs/2026-06-05-keymap-builtin-disambiguation-design.md`.

use std::collections::HashMap;

use nxvim_core::{
    command_status, key_to_notation, parse_keys, CommandStatus, Key, KeyContext, Mode,
};
use nxvim_lua::{RawKeymap, RawRhs};

/// What the matcher is matching *against* on a given keystroke — the buffer in some
/// editor [`Mode`], or a grabbing widget in its own keymap bucket.
///
/// An `Editing` scope is the full editing matcher: bucket `mode_key(mode)`, the
/// [`command_status`] disambiguation oracle on the break path, and (in the server)
/// the literal-argument bypass. A `Widget` scope drives the *same* withhold/replay
/// trie over the widget's bucket but with **no oracle** (a widget has no core
/// command grammar to disambiguate against) — a withheld prefix that breaks simply
/// replays raw to the widget's handler. The bucket char pairs with
/// [`mode_buckets`] / [`widget_bucket`].
#[derive(Clone, Copy)]
pub enum MatchScope {
    /// The buffer, in this mode — the per-mode trie + oracle.
    Editing(Mode),
    /// A grabbing widget, in this bucket — the trie only, no oracle.
    Widget(char),
}

impl MatchScope {
    /// The trie bucket this scope matches in.
    fn bucket(self) -> char {
        match self {
            MatchScope::Editing(mode) => mode_key(mode),
            MatchScope::Widget(bucket) => bucket,
        }
    }

    /// The editor mode for the disambiguation oracle, or `None` for a widget scope
    /// (where the oracle does not apply).
    fn oracle_mode(self) -> Option<Mode> {
        match self {
            MatchScope::Editing(mode) => Some(mode),
            MatchScope::Widget(_) => None,
        }
    }

    /// The readable mode code reported to the [`KeyPending`] event (which-key) — the
    /// editor mode's short code for an editing scope, or the widget's keymap mode
    /// name for a widget scope, so a which-key consumer can tell it is showing a
    /// picker / select / panel key table. The widget arm is the inverse of
    /// [`mode_buckets`] / [`widget_bucket`] and must stay in sync with them.
    pub fn mode_code(self) -> &'static str {
        match self {
            MatchScope::Editing(mode) => mode.short_code(),
            MatchScope::Widget('P') => "picker",
            MatchScope::Widget('S') => "select",
            MatchScope::Widget('L') => "panel",
            MatchScope::Widget(_) => "",
        }
    }
}

/// The keymap bucket char a grabbing-widget [`KeyContext`] routes through, or `None`
/// for [`KeyContext::Editing`] (the buffer's per-mode trie). Pairs with
/// [`mode_buckets`], which lands a `vim.keymap.set('picker', …)` in the same bucket
/// so its default maps and a user override compile into the trie this selects.
pub fn widget_bucket(ctx: KeyContext) -> Option<char> {
    match ctx {
        KeyContext::Editing => None,
        KeyContext::Picker => Some('P'),
        KeyContext::Select => Some('S'),
        KeyContext::Panel => Some('L'),
    }
}

/// What a matched mapping does when it fires (design D7). The fire dispatch is a
/// `match` over this enum.
#[derive(Clone, Debug)]
pub enum MappingRhs {
    /// A Lua function RHS, keyed by id in `nx._keymap_fns`; the server runs it
    /// via `LuaRuntime::run_keymap` and folds in the effects.
    Lua(u64),
    /// A string RHS already parsed to keys, with its `noremap` flag. A `noremap`
    /// RHS is fed straight to the editor (bypassing the trie); a remap RHS is
    /// re-fed *through the matcher* so its keys can themselves trigger mappings
    /// (see [`Keymaps::fire`], bounded by the `remap_budget`).
    Keys(Vec<Key>, bool),
}

/// A compiled mapping at a trie node: its RHS plus the option flags that change
/// how it matches (`nowait`) or how its effects surface (`silent`).
#[derive(Clone, Debug)]
pub struct Mapping {
    /// What fires when this mapping matches.
    pub rhs: MappingRhs,
    /// `<nowait>`: fire the moment this mapping completes, even if it is also a
    /// prefix of a longer one — don't hold it waiting for the longer map (which,
    /// timer-less, would otherwise need the idle flush or the next key to resolve).
    pub nowait: bool,
    /// `<silent>`: the message line this mapping's execution produces is suppressed
    /// (the server snapshots/restores it around the fire); `:messages` is unaffected.
    pub silent: bool,
    /// `<expr>`: a function RHS computes the keys to feed rather than acting; the
    /// server runs it via `run_keymap_expr` and feeds the returned keys. Only a Lua
    /// RHS is affected — a string RHS ignores it (nxvim has no expression evaluator).
    pub expr: bool,
    /// The `desc` opt, surfaced to the [`KeyPending`] event (which-key / showcmd) as
    /// a continuation's label. `None` for a map with no description. Unused by
    /// matching.
    pub desc: Option<String>,
}

/// A snapshot of the live pending key-context the **`KeyPending`** event carries to
/// Lua (which-key / showcmd): the mode it was computed in, the withheld prefix in
/// vim notation, and the immediate continuations that extend it. Built by
/// [`Keymaps::pending_context`] from the mapped-prefix trie — source A (user maps,
/// plus the LSP plugin's on-attach maps). The built-in command grammar (`g`/`z`/operator-pending)
/// and active-widget key tables (sources B/C of the design) are a later extension;
/// this is the engine signal that unblocks a mapped-prefix (leader-key) which-key.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyPending {
    /// The editor-mode short code the prefix is live in (`"n"`, `"i"`, `"v"`, …).
    pub mode: String,
    /// The withheld prefix as re-parseable vim notation (`"g"`, `"<Space>w"`).
    pub keys: String,
    /// Every immediate continuation of the prefix, sorted by key notation so the
    /// event payload is deterministic (the trie's child map is unordered). Empty for
    /// a **source B** built-in pending state (find-char, marks, …), whose
    /// continuation set is open — those carry a [`label`](Self::label) instead.
    pub continuations: Vec<Continuation>,
    /// A human-readable hint for an **open** pending state — the built-in command
    /// grammar's source-B leaves (`"Find character"`, `"Replace character"`), where
    /// there is no finite continuation list to show. `None` for sources A/C (mapped
    /// prefixes), which enumerate `continuations` instead. which-key renders this
    /// when `continuations` is empty.
    pub label: Option<String>,
}

/// One key that extends a pending prefix in the [`KeyPending`] event.
#[derive(Clone, Debug, PartialEq)]
pub struct Continuation {
    /// The continuation key as vim notation (`"w"`, `"<C-w>"`).
    pub key: String,
    /// The `desc` of the mapping this key completes, when it completes one.
    pub desc: Option<String>,
    /// Whether this key completes a mapping or only leads deeper into the trie.
    pub kind: ContinuationKind,
    /// Whether this continuation is still reachable in the current state. `true` for
    /// every live continuation; `false` only for a **stale** mapped continuation kept
    /// visible for legibility — a `g`-prefix map (`gd`/`gD`/`gr`) surfaced *after* the
    /// leader timeout committed `g` to the built-in grammar, so the map can no longer
    /// fire. which-key dims / cues these rather than dropping them mid-popup.
    pub available: bool,
}

/// Whether a [`Continuation`] completes a mapping or only extends toward one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuationKind {
    /// The key completes a mapping (it may *also* lead deeper to a longer one).
    Map,
    /// The key only extends toward longer mappings — no mapping ends on it. A
    /// which-key popup renders these as a `+prefix` group.
    Group,
}

impl ContinuationKind {
    /// The lowercase tag the Lua event payload carries (`"map"` / `"group"`).
    pub fn as_str(self) -> &'static str {
        match self {
            ContinuationKind::Map => "map",
            ContinuationKind::Group => "group",
        }
    }
}

/// A unit of work [`Keymaps::feed`] hands back for the server to apply, in order.
pub enum Step {
    /// Send this key to `editor.input` (then `emit_lifecycle_events`).
    Editor(Key),
    /// Fire this mapping's RHS; `silent` carries the `<silent>` flag the server
    /// honors by restoring the message line after the fire, and `expr` the
    /// `<expr>` flag (run the Lua RHS for its returned keys, then feed them).
    Fire {
        rhs: MappingRhs,
        silent: bool,
        expr: bool,
    },
}

/// A node in a per-mode prefix trie: the mapping that ends here (if any) and the
/// continuations that extend it.
#[derive(Default)]
struct Node {
    children: HashMap<Key, Node>,
    mapping: Option<Mapping>,
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
    /// A complete mapping to fire now — either nothing longer extends it, or it is
    /// `<nowait>` so it fires without waiting for a longer continuation.
    Complete(Mapping),
    /// Not a prefix of anything: the sequence broke every live mapping.
    None,
}

impl Trie {
    /// Insert `keys` → `mapping`. A later insert at the same path overwrites an
    /// earlier one, which is how the precedence ladder resolves to last-wins
    /// (callers insert lowest-precedence first; see [`Keymaps::build_for`]).
    fn insert(&mut self, keys: &[Key], mapping: Mapping) {
        let mut node = &mut self.root;
        for k in keys {
            node = node.children.entry(*k).or_default();
        }
        node.mapping = Some(mapping);
    }

    /// Classify `keys` against the trie. A mapping that is *also* a prefix of a
    /// longer one (ambiguous, e.g. `j` & `jk`) is normally held as `Prefix` and
    /// resolved when a later key breaks it (via [`Trie::longest_complete`]) — unless
    /// it is `<nowait>`, in which case it fires the moment it completes, ignoring
    /// the longer continuation (vim's `<nowait>` semantics).
    fn classify(&self, keys: &[Key]) -> Classify {
        let mut node = &self.root;
        for k in keys {
            match node.children.get(k) {
                Some(n) => node = n,
                None => return Classify::None,
            }
        }
        match (&node.mapping, node.children.is_empty()) {
            (Some(m), true) => Classify::Complete(m.clone()), // complete, nothing longer
            (Some(m), false) if m.nowait => Classify::Complete(m.clone()), // nowait: fire now
            (Some(_), false) => Classify::Prefix,             // complete but also a prefix: hold
            (None, false) => Classify::Prefix,                // live prefix, not yet complete
            (None, true) => Classify::None,                   // unreachable in a well-formed trie
        }
    }

    /// The immediate continuations of `keys` — the children of the trie node `keys`
    /// leads to — or `None` when `keys` is not a live prefix in this trie. A child
    /// that terminates a mapping is a [`Map`](ContinuationKind::Map) carrying its
    /// `desc`; one that only leads deeper is a [`Group`](ContinuationKind::Group).
    /// Sorted by key notation so the [`KeyPending`] payload is deterministic.
    fn continuations(&self, keys: &[Key]) -> Option<Vec<Continuation>> {
        let mut node = &self.root;
        for k in keys {
            node = node.children.get(k)?;
        }
        let mut out: Vec<Continuation> = node
            .children
            .iter()
            .map(|(k, child)| {
                let (kind, desc) = match &child.mapping {
                    Some(m) => (ContinuationKind::Map, m.desc.clone()),
                    None => (ContinuationKind::Group, None),
                };
                Continuation {
                    key: key_to_notation(*k),
                    desc,
                    kind,
                    available: true,
                }
            })
            .collect();
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Some(out)
    }

    /// The longest prefix of `keys` that is a complete mapping, with its length —
    /// used to fire the shorter map when an ambiguous sequence finally breaks.
    fn longest_complete(&self, keys: &[Key]) -> Option<(Mapping, usize)> {
        let mut node = &self.root;
        let mut best = None;
        for (i, k) in keys.iter().enumerate() {
            match node.children.get(k) {
                Some(n) => {
                    node = n;
                    if let Some(m) = &node.mapping {
                        best = Some((m.clone(), i + 1));
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
    /// `nx._keymaps_version` the cached [`snapshot`](Self::snapshot) was last
    /// pulled at. The server re-reads the registry only when the live version
    /// advances (checked once per batch).
    pub version: u64,
    /// The registry snapshot, pre-sorted into precedence order (lowest first), so
    /// [`build_for`](Self::build_for) can compile a buffer's tries by a single
    /// linear insert without re-sorting. Kept across buffer switches: a switch
    /// rebuilds the tries from this same snapshot, filtered to the new buffer.
    snapshot: Vec<RawKeymap>,
    /// The buffer the cached `tries` were built for, or `None` when they need a
    /// (re)build — set on a version bump or after a buffer switch. Buffer-local
    /// maps for *other* buffers are filtered out of the tries (design D6's
    /// buffer-local > global rung), so this gates a rebuild when the current
    /// buffer changes even though the registry version did not.
    built_buffer: Option<u64>,
    /// Per-mode tries, keyed by the editor-mode code the matcher selects them by
    /// (`mode_key`: `'n'`, `'i'`, `'v'`, `'V'`, …). A map's declared mode expands
    /// to one or more of these buckets at build time (see [`mode_buckets`]).
    tries: HashMap<char, Trie>,
    /// Keys withheld as a live prefix, awaiting the key that extends, completes,
    /// or breaks them. Persists across batches (no auto-flush — design D4).
    pending: Vec<Key>,
    /// Remaining recursive-`remap` re-feeds for the current keystroke (vim's
    /// `maxmapdepth`). Reset at the top of every [`feed`](Keymaps::feed) and
    /// decremented on each remap expansion, so a self-referential map (`a`→`a`,
    /// or `a`↔`b`) runs out of budget and falls through to a literal key instead
    /// of looping. It is a *shared* budget across the whole expansion tree, not a
    /// per-branch depth, so a fan-out remap (`a`→`bb`, `b`→`a`) stays linear.
    remap_budget: usize,
}

/// The recursive-remap re-feed cap (vim's `maxmapdepth` is 1000; a smaller cap is
/// plenty — it only has to break self-referential maps cleanly).
const MAX_MAP_DEPTH: usize = 100;

impl Keymaps {
    /// Cache a fresh registry snapshot (read when `nx._keymaps_version`
    /// advanced) and remember its version. Entries are sorted once into
    /// precedence order — **buffer-local > global**, within a scope **user
    /// (non-default) > default**, and among equals **last-set wins** — so
    /// [`build_for`](Self::build_for) can replay them lowest-first and let
    /// higher-precedence entries overwrite at the same LHS path (D6). Marks the
    /// tries stale (via [`needs_build`](Self::needs_build)) so the server rebuilds
    /// them before the next match.
    pub fn set_snapshot(&mut self, version: u64, mut snapshot: Vec<RawKeymap>) {
        self.version = version;
        snapshot.sort_by_key(|e| (e.buffer.is_some(), !e.default, e.seq));
        self.snapshot = snapshot;
        self.built_buffer = None;
    }

    /// Whether the cached tries need a (re)build for `buffer` — true after a
    /// version bump (which clears [`built_buffer`](Self::built_buffer)) or when the
    /// current buffer differs from the one the tries were built for.
    pub fn needs_build(&self, buffer: u64) -> bool {
        self.built_buffer != Some(buffer)
    }

    /// (Re)compile the per-mode tries for `buffer` from the cached snapshot,
    /// keeping global maps and the buffer-local maps scoped to `buffer` while
    /// dropping buffer-local maps for *other* buffers. Because the snapshot is
    /// pre-sorted (buffer-local last within a scope), the surviving buffer-local
    /// entries overwrite the globals at the same LHS — the buffer-local > global
    /// rung of D6. Phase 1 exercised only last-set-wins (all global); this is
    /// where the buffer rung first does real work.
    pub fn build_for(&mut self, buffer: u64) {
        self.built_buffer = Some(buffer);
        self.tries.clear();
        for entry in &self.snapshot {
            // Skip buffer-local maps that belong to a different buffer.
            if matches!(entry.buffer, Some(b) if b != buffer) {
                continue;
            }
            let lhs = parse_keys(&entry.lhs);
            if lhs.is_empty() {
                continue;
            }
            let rhs = match &entry.rhs {
                RawRhs::Lua(id) => MappingRhs::Lua(*id),
                RawRhs::Str(s) => MappingRhs::Keys(parse_keys(s), entry.noremap),
            };
            let mapping = Mapping {
                rhs,
                nowait: entry.nowait,
                silent: entry.silent,
                expr: entry.expr,
                desc: entry.desc.clone(),
            };
            for mode in &entry.modes {
                // A declared map-mode (`'n'`, `'v'`/`'x'`, `''` = all, …) fans out
                // to the editor-mode tries it covers — `'v'` lands in both the
                // Visual and Visual-Line tries, `''` in normal+visual, etc.
                for &bucket in mode_buckets(mode) {
                    self.tries
                        .entry(bucket)
                        .or_default()
                        .insert(&lhs, mapping.clone());
                }
            }
        }
    }

    /// Whether nothing is currently withheld in the prefix buffer. The server
    /// checks this before bypassing the matcher for a core literal-argument key
    /// (see [`crate::EditHost::feed_matcher`]): a literal arg only arises after its
    /// lead key (`r`/`f`/`"`/…) already reached the editor, which leaves `pending`
    /// empty — the guard makes that invariant explicit so a bypass can never
    /// reorder past a genuinely-withheld prefix.
    pub fn pending_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// The live pending key-context for `scope` (the **`KeyPending`** oracle), or
    /// `None` when nothing is withheld — the withheld prefix plus its continuations
    /// from the current buffer's mapped-prefix trie. A `None` (empty `pending`) tells
    /// the caller there is no active prefix, which it emits as a *cleared* event so a
    /// which-key popup closes. Computed against the current trie, so it already
    /// reflects buffer-local maps and the precedence ladder the matcher matches on.
    /// `scope` selects the bucket so a grabbing widget's prefix lists *its* keys
    /// (source C) — a multi-key `picker`/`panel`/… map shows under that widget — while
    /// an editing scope lists the per-mode keys as before.
    pub fn pending_context(&self, scope: MatchScope) -> Option<KeyPending> {
        if self.pending.is_empty() {
            return None;
        }
        let continuations = self
            .tries
            .get(&scope.bucket())?
            .continuations(&self.pending)?;
        Some(KeyPending {
            mode: scope.mode_code().to_string(),
            keys: self.pending.iter().copied().map(key_to_notation).collect(),
            continuations,
            // Sources A/C enumerate continuations; the label is the source-B channel.
            label: None,
        })
    }

    /// The trie continuations of an *explicit* key path in `scope`, independent of the
    /// live `pending` — for surfacing mapped continuations the matcher is no longer
    /// withholding. The server uses this to keep a `g`-prefix's maps (`gd`/`gD`/`gr`)
    /// visible *after* the leader timeout released `g` into the built-in grammar: the
    /// caller flags them [`available = false`](Continuation::available), since they can
    /// no longer fire from this state. Empty when `keys` is not a live prefix here.
    pub fn continuations_at(&self, scope: MatchScope, keys: &[Key]) -> Vec<Continuation> {
        self.tries
            .get(&scope.bucket())
            .and_then(|t| t.continuations(keys))
            .unwrap_or_default()
    }

    /// Feed one input key in `scope` and return the steps it produced. The server
    /// calls this for every parsed key, executing the steps in order. `scope` is the
    /// buffer's [`Mode`](MatchScope::Editing) for ordinary editing, or a grabbing
    /// widget's [`bucket`](MatchScope::Widget) while a widget owns input.
    pub fn feed(&mut self, scope: MatchScope, key: Key) -> Vec<Step> {
        // Fresh remap budget per real keystroke (vim resets `maxmapdepth` once a
        // typed char is consumed; a keystroke is exactly that boundary).
        self.remap_budget = MAX_MAP_DEPTH;
        let mut steps = Vec::new();
        self.feed_key(scope, key, &mut steps);
        steps
    }

    /// Resolve whatever is withheld in `pending` *as if a `timeoutlen` boundary
    /// had passed* — the synthetic idle flush that closes the D4 gap (design §3 /
    /// D4). The TUI sends this after an idle gap with no following key; the server
    /// turns it into a [`flush`](Keymaps::flush) so a trailing live-prefix fires
    /// without waiting for the *next* keystroke.
    ///
    /// Semantics are the next-key break path with no next key: fire the longest
    /// complete mapping that prefixes the buffer (the ambiguous *shorter* map —
    /// e.g. `j` when both `j` and `jk` are mapped), then re-feed the remainder; with
    /// no complete prefix the withheld keys were not a mapping, so replay them raw
    /// (e.g. the second `g` of `gg` reaches the editor, completing go-to-top). Empty
    /// `pending` makes this a no-op, so the client can flush unconditionally on idle.
    ///
    /// The loop drains any remainder a re-feed re-withholds (overlapping maps can
    /// leave a deeper prefix buffered); it terminates because each pass consumes at
    /// least one key, so `pending` strictly shrinks. The defensive break guards the
    /// invariant in case that ever fails to hold.
    pub fn flush(&mut self, scope: MatchScope) -> Vec<Step> {
        self.remap_budget = MAX_MAP_DEPTH;
        let mut steps = Vec::new();
        let bucket = scope.bucket();
        while !self.pending.is_empty() {
            let before = self.pending.len();
            let buffered: Vec<Key> = self.pending.drain(..).collect();
            if self.tries.contains_key(&bucket) {
                // No next key on the flush path, so the break-path oracle does not
                // apply here: a trailing live-prefix replays raw and the editor
                // completes any built-in itself (the re-feeds inside `resolve_
                // buffered` still route through `feed_key`, where the oracle runs).
                let _ = self.resolve_buffered(scope, &buffered, &mut steps);
            } else {
                // No trie for this scope (the prefix was withheld in another mode /
                // widget, then the context changed): the keys can't be a mapping here.
                steps.extend(buffered.into_iter().map(Step::Editor));
            }
            if self.pending.len() >= before {
                steps.extend(self.pending.drain(..).map(Step::Editor));
                break;
            }
        }
        steps
    }

    fn feed_key(&mut self, scope: MatchScope, key: Key, steps: &mut Vec<Step>) {
        let bucket = scope.bucket();
        // No mappings for this scope: flush any prefix buffered in another and pass
        // the key straight through. (In practice `pending` is empty here.)
        if !self.tries.contains_key(&bucket) {
            steps.extend(self.pending.drain(..).map(Step::Editor));
            steps.push(Step::Editor(key));
            return;
        }
        self.pending.push(key);
        let classify = self.tries[&bucket].classify(&self.pending);
        match classify {
            Classify::Prefix => {} // hold: wait for the next key
            Classify::Complete(mapping) => {
                self.pending.clear();
                self.fire(scope, mapping, steps);
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
                    let raw = self.resolve_buffered(scope, &buffered, steps);
                    // Disambiguation oracle (design §"Matcher integration"), editing
                    // scopes only — a widget has no core command grammar (`oracle_
                    // mode()` is `None` for `Widget`), so a broken widget prefix just
                    // replays raw and re-feeds. The buffered prefix just replayed
                    // *raw* to the editor (no shorter mapping fired — gate `raw.is_
                    // some()`). If re-feeding `key` would only re-withhold it as a
                    // fresh mapping prefix, but that raw run *plus* `key` already forms
                    // a complete built-in, release `key` to the editor at once — so a
                    // built-in like `gg` (under a colliding `gh` map) reaches the
                    // editor whole and fires instantly. The `would_hold` guard keeps
                    // user maps winning.
                    if let (Some(raw_run), Some(mode)) = (raw, scope.oracle_mode()) {
                        let would_hold = matches!(
                            self.tries[&bucket].classify(std::slice::from_ref(&key)),
                            Classify::Prefix
                        );
                        if would_hold {
                            let mut run = raw_run;
                            run.push(key);
                            if command_status(mode, &run) == CommandStatus::Complete {
                                steps.push(Step::Editor(key));
                                return;
                            }
                        }
                    }
                    self.feed_key(scope, key, steps);
                }
            }
        }
    }

    /// Execute a fired mapping. A `remap` string RHS is re-fed key-by-key *through
    /// the matcher* so its keys can themselves trigger further mappings, bounded by
    /// the shared `remap_budget` so a self-referential map terminates (at the cap
    /// the remaining keys fall through as a literal feed). Everything else — a
    /// `noremap` string RHS, a Lua function, a future native action — is handed to
    /// the server as a [`Step::Fire`] (a `noremap` RHS the server feeds straight to
    /// the editor; a Lua/native RHS it invokes), carrying the `<silent>`/`<expr>`
    /// flags the server honors.
    ///
    /// `<silent>` on a *remap* RHS is not threaded onto the re-fed keys: the inner
    /// maps they trigger surface with their own flags, matching the fact that the
    /// outer map's effect is just to type those keys. The flag bites on the terminal
    /// fire (Lua / `noremap` string), which is where a message would be produced.
    fn fire(&mut self, scope: MatchScope, mapping: Mapping, steps: &mut Vec<Step>) {
        match mapping.rhs {
            MappingRhs::Keys(keys, false) if self.remap_budget > 0 => {
                self.remap_budget -= 1;
                for key in keys {
                    self.feed_key(scope, key, steps);
                }
            }
            rhs => steps.push(Step::Fire {
                rhs,
                silent: mapping.silent,
                expr: mapping.expr,
            }),
        }
    }

    /// Resolve a run of buffered keys that no longer extends any mapping: fire the
    /// longest complete mapping that prefixes them (the ambiguous shorter map),
    /// then re-process the remainder through the matcher (it follows a completed
    /// map and may itself begin a new one — strictly shorter than `buffered`, so
    /// this terminates). With no complete prefix the withheld keys were not a
    /// mapping at all: replay them to the editor raw (re-feeding them would just
    /// re-withhold the same live prefix and loop).
    ///
    /// Returns `Some(raw)` with the keys replayed raw when no shorter mapping
    /// fired — the "released run" the break-path oracle splices the next key onto
    /// — and `None` when a mapping fired (the raw-replay gate: a fired-then-
    /// leftover run is not a clean built-in command sequence, so it never reaches
    /// the oracle).
    fn resolve_buffered(
        &mut self,
        scope: MatchScope,
        buffered: &[Key],
        steps: &mut Vec<Step>,
    ) -> Option<Vec<Key>> {
        match self.tries[&scope.bucket()].longest_complete(buffered) {
            Some((mapping, used)) => {
                self.fire(scope, mapping, steps);
                for &key in &buffered[used..] {
                    self.feed_key(scope, key, steps);
                }
                None
            }
            None => {
                steps.extend(buffered.iter().copied().map(Step::Editor));
                Some(buffered.to_vec())
            }
        }
    }
}

/// The trie key the matcher selects for the *current editor mode* — the first
/// char of its `mode()` short code (`Normal` → `'n'`, `Insert` → `'i'`,
/// `Visual` → `'v'`, `VisualLine` → `'V'`, `Command` → `'c'`). Paired with
/// [`mode_buckets`], which decides which of these tries a declared map lands in.
///
/// Multi-cursor *placement* mode is the one divergence from `short_code`: it
/// reports `"n"` so `mode()`-checking scripts read it as normal, but it selects
/// its **own** `'m'` trie so a `vim.keymap.set('m', …)` map fires only while
/// placing and a plain `'n'` map does not leak in (the placement grammar still
/// reaches the editor untouched whenever the `'m'` trie has no match).
fn mode_key(mode: Mode) -> char {
    if mode == Mode::MultiCursor {
        return 'm';
    }
    mode.short_code().chars().next().unwrap_or('n')
}

/// The editor-mode trie buckets a declared map-mode code lands in — the build-time
/// counterpart of [`mode_key`]. A `'v'`/`'x'` map covers both Visual and
/// Visual-Line (nxvim has no Select mode, so they coincide); `'m'` is the
/// nxvim-specific multi-cursor *placement* bucket; `''` (vim's `:map`) covers
/// normal + visual + placement (every normal-ish mode). Operator-pending (`'o'`)
/// is **deferred**: nxvim has no
/// operator-pending *mode* (a pending operator lives in private core state while
/// `editor.mode == Normal`), so there is no trie to select it by — the normal-trie
/// replay path already preserves `d{motion}`/`dgg`, so a dedicated `omap` trie is
/// deferred. An unknown code maps to nothing.
fn mode_buckets(code: &str) -> &'static [char] {
    match code {
        "n" => &['n'],
        "i" => &['i'],
        "c" => &['c'],
        "v" | "x" => &['v', 'V'],
        "m" => &['m'],
        // Grabbing-widget buckets (configurable widget keys): a `vim.keymap.set
        // ('picker', …)` lands here, and the matcher selects it via a `Widget` scope
        // while that widget grabs input (see [`widget_bucket`]). Distinct from every
        // editor-mode bucket above so widget maps never leak into editing and vice
        // versa. picker / select / panel; cmdline reuses the `c` bucket. (The
        // explorer / `nx.view` / quickfix buffers are ordinary buffers with
        // buffer-local maps, not widget buckets — see the unify-special-buffers plan.)
        "picker" => &['P'],
        "select" => &['S'],
        "panel" => &['L'],
        // The command line reuses the existing command-mode bucket (`mode_key(Mode::
        // Command) == 'c'`); `cmdline` is just the readable alias for it, so its
        // default maps and a user `set('c', …)` compile into the same trie.
        "cmdline" => &['c'],
        "" => &['n', 'v', 'V', 'm'],
        _ => &[],
    }
}
