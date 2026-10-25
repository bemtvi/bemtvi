//! Branching undo/redo: the per-buffer [`UndoTree`] and the editor operations
//! that navigate it.
//!
//! The model is full-snapshot-per-node (cheap thanks to `ropey`'s persistent
//! rope — see [`Snapshot`]), organized as a tree rather than two stacks. Undoing
//! then making a new edit forks a branch instead of discarding the old future,
//! so every state stays reachable: a plain `u`/`<C-r>` walks parent/newest-child
//! as before, while `:undo {N}` jumps to any seq across branches.
//!
//! A pending edit is committed to a node *lazily*, at the moment focus leaves the
//! state (the next change-group boundary, or an undo/redo/`:undo`). That is the
//! same instant the old two-stack model snapshotted, so cursor/text/seq land
//! identically; the tree just keeps the branches the stacks threw away.

use super::*;

/// What a typed key is about to do to the text. An undo step never mixes the two:
/// typing then `<BS>` undoes as two steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditKind {
    Insert,
    Delete,
}

/// Where an insert session's typing stands, for word-sized undo steps (see
/// [`Editor::before_typed_edit`]). Reset whenever an insert session opens.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TypingUndo {
    /// The kind of the last typed edit; `None` until the session's first one.
    kind: Option<EditKind>,
    /// The last character typed, when the last edit inserted one.
    last: Option<char>,
    /// The buffer's `changedtick` when the current step opened. A step that has
    /// not changed the text is never committed, so a no-op key (`<BS>` at the
    /// start of the buffer) can't leave an empty step behind.
    tick: u64,
    /// The next typed edit opens a new step whatever it is — set after a paste
    /// or an `i_CTRL-O` command, so typing that follows one undoes on its own.
    force: bool,
    /// [`Editor::now_ms`] at the last typed edit, for the pause rule.
    last_ms: u64,
    /// Where the cursor was left by the last typed edit (`(line, col)`). If it is
    /// somewhere else when the next one arrives — an arrow, a click, a jump, a
    /// plugin moving it — that edit starts a new step.
    at: Option<(usize, usize)>,
}

/// How long typing may pause, in milliseconds, before the next edit starts a new
/// undo step even mid-word.
const UNDO_PAUSE_MS: u64 = 1000;

/// Whether typing `next` right after `last` starts a new undo step: the first
/// separator after a word (it goes with the word that follows), or the first
/// character after a line break (the break went with the line it ended).
fn starts_typing_step(last: Option<char>, next: Option<char>) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    match (last, next) {
        (Some('\n'), Some(_)) => true,
        (Some(p), Some(n)) => is_word(p) && !is_word(n) && n != '\n',
        _ => false,
    }
}

impl UndoTree {
    /// A fresh tree whose only node is the original buffer state at seq 0.
    pub(crate) fn new(buffer: &Buffer) -> Self {
        let root = UndoNode {
            seq: 0,
            parent: None,
            children: Vec::new(),
            time: 0,
            save: None,
            snap_extmark_gen: buffer.extmarks.generation(),
            snap: Snapshot {
                text: buffer.text.clone(),
                cursor: Cursor::default(),
                extmarks: buffer.extmarks.clone(),
                marks: buffer.marks.clone(),
                changelist: (buffer.changelist.clone(), buffer.changelistidx),
                cursor_window: None,
                selection: None,
            },
        };
        UndoTree {
            nodes: vec![root],
            cur: 0,
            next_seq: 1,
            dirty: false,
            dirty_since: 0,
            dirty_tick: 0,
            save_last: 0,
        }
    }

    /// Mark that a change group has begun (the live buffer will diverge from
    /// `cur`), remembering when, for the virtual-node timestamp. Idempotent within
    /// a group — a re-mark keeps the original start time.
    fn mark_dirty(&mut self, now: i64, tick: u64) {
        if !self.dirty {
            self.dirty = true;
            self.dirty_since = now;
            self.dirty_tick = tick;
        }
    }

    /// Whether the live buffer holds a change not yet committed: a group is open
    /// *and* the buffer's `changedtick` (`tick`) has moved since it began. An open
    /// group that changed nothing is not a state, so it is neither committed nor
    /// shown.
    fn pending(&self, tick: u64) -> bool {
        self.dirty && tick != self.dirty_tick
    }

    /// Re-sync the current node's snapshot with the live state we are about to edit
    /// away from — see [`Editor::push_undo`]. The cursor and the multi-cursor
    /// ([`CURSOR_NS`]) marks are taken from the arguments; `marks` is the live
    /// per-file mark store, and `extmarks` the live extmark store — the latter only
    /// when it has structurally moved since this node was last synced (`Some`), see
    /// [`Editor::refresh_undo_cursor_marks`].
    ///
    /// Refreshing the *marks* matters because a node is snapshotted when the state is
    /// **entered**, and marks are routinely added to a state afterwards, outside any
    /// `push_undo`: the root node is captured at buffer load, and a `BufReadPost`
    /// handler decorates the buffer a moment later. Undoing back to a node
    /// committed before those marks existed wiped every one of them — the first `u`
    /// after opening a decorated file threw the decorations away. The live store is
    /// in the same coordinate space as the snapshot's text (no edit has happened
    /// yet), so it can be taken wholesale.
    fn set_cur_snapshot_cursors(
        &mut self,
        primary: Cursor,
        positions: &[usize],
        window: Option<WindowId>,
        extmarks: Option<crate::extmark::ExtmarkStore>,
        marks: std::collections::HashMap<char, (usize, usize)>,
        live_extmark_gen: u64,
    ) {
        let node = &mut self.nodes[self.cur];
        node.snap_extmark_gen = live_extmark_gen;
        let snap = &mut node.snap;
        snap.cursor = primary;
        snap.cursor_window = window;
        if let Some(extmarks) = extmarks {
            snap.extmarks = extmarks;
        }
        snap.marks = marks;
        snap.extmarks.clear(crate::extmark::CURSOR_NS, None);
        for &at in positions {
            snap.extmarks
                .set(crate::extmark::CURSOR_NS, None, at, None, None, 0, None);
        }
    }

    /// The live extmark generation the current node's snapshot was last synced to —
    /// see [`UndoNode::snap_extmark_gen`].
    fn cur_snapshot_extmark_gen(&self) -> u64 {
        self.nodes[self.cur].snap_extmark_gen
    }

    /// Seq of the current state (`b_u_seq_cur`).
    pub(crate) fn cur_seq(&self) -> u64 {
        self.nodes[self.cur].seq
    }

    /// Highest seq minted so far (`b_u_seq_last`).
    fn seq_last(&self) -> u64 {
        self.next_seq - 1
    }

    /// The node holding state `seq`, if any.
    fn node_of_seq(&self, seq: u64) -> Option<NodeIdx> {
        self.nodes.iter().position(|n| n.seq == seq)
    }

    /// Whether the state at `idx` lies on the `back` side of the current one in
    /// **seq** order — at or before `cur` going back, strictly after it going
    /// forward. Every travel filters its candidates through this before looking at
    /// the field it actually seeks by; vim wraps its own closest-match search in the
    /// same test (`uh_seq <= b_u_seq_cur` going back, `uh_seq > b_u_seq_cur` going
    /// forward).
    ///
    /// It is what keeps a travel monotonic when the sought field is *not*. A save
    /// number is stamped onto whichever state is current when `:w` runs, so it does
    /// not grow with seq: write, undo, write again, and write 1 sits on a *later*
    /// state than write 2 — seeking "one write back" by number alone would walk
    /// forward into the abandoned branch. Node times can tie the same way (the
    /// timeline is whole seconds).
    fn on_travel_side(&self, idx: NodeIdx, back: bool) -> bool {
        let cur = self.cur_seq();
        if back {
            self.nodes[idx].seq <= cur
        } else {
            self.nodes[idx].seq > cur
        }
    }

    /// The node holding the state **nearest** `target` in seq order, on the `back`
    /// side of it: the greatest `seq <= target` going back, the smallest `seq >=
    /// target` going forward.
    ///
    /// Seeks rather than requiring an exact hit because seqs are not guaranteed
    /// dense — `'undolevels'` pruning drops the oldest states without renumbering
    /// the survivors, so a target can name a state that no longer exists. Falls back
    /// to the nearest node on the *other* side when nothing lies on the requested one
    /// (every reachable end of a pruned tree still lands somewhere real).
    fn node_near_seq(&self, target: u64, back: bool) -> NodeIdx {
        let on_side = self.nodes.iter().enumerate().filter(|&(i, n)| {
            self.on_travel_side(i, back)
                && if back {
                    n.seq <= target
                } else {
                    n.seq >= target
                }
        });
        let pick = if back {
            on_side.max_by_key(|(_, n)| n.seq)
        } else {
            on_side.min_by_key(|(_, n)| n.seq)
        };
        match pick {
            Some((i, _)) => i,
            // Nothing that far (a pruned-away stretch of seqs): take the closest
            // state still in the travel's direction rather than refusing to move.
            None => self
                .nodes
                .iter()
                .enumerate()
                .filter(|&(i, _)| self.on_travel_side(i, back))
                .min_by_key(|(_, n)| n.seq.abs_diff(target))
                .map(|(i, _)| i)
                .unwrap_or(self.cur),
        }
    }

    /// The node whose `time` is nearest `target` on the `back` side of it — the
    /// [`node_near_seq`](Self::node_near_seq) rule applied to timestamps, over the
    /// candidates [`on_travel_side`](Self::on_travel_side) admits, and tie-broken by
    /// seq (the timeline is whole seconds, so ties are the common case).
    fn node_near_time(&self, target: i64, back: bool) -> NodeIdx {
        let on_side = self.nodes.iter().enumerate().filter(|&(i, n)| {
            self.on_travel_side(i, back)
                && if back {
                    n.time <= target
                } else {
                    n.time >= target
                }
        });
        let pick = if back {
            on_side.max_by_key(|(_, n)| (n.time, n.seq))
        } else {
            on_side.min_by_key(|(_, n)| (n.time, n.seq))
        };
        match pick {
            Some((i, _)) => i,
            // The whole tree lies past `target`: travel as far as it goes.
            None => self.end_node(back),
        }
    }

    /// The save number of `cur`, else of the nearest **ancestor** that was written —
    /// "the write this state descends from". An ancestor walk rather than a seq
    /// comparison, because a write on an abandoned branch is not behind us.
    fn save_at_or_above_cur(&self) -> Option<u64> {
        let mut at = self.cur;
        loop {
            if let Some(nr) = self.nodes[at].save {
                return Some(nr);
            }
            at = self.nodes[at].parent?;
        }
    }

    /// The node stamped with save number `target`, or the nearest write on the `back`
    /// side of it — among the candidates [`on_travel_side`](Self::on_travel_side)
    /// admits, without which a write stamped on a *newer* state could be sought
    /// backwards. A target past either end (including `<= 0`, "before the first
    /// write"), or no write left in the travel's direction, resolves to that end of
    /// the tree, so `:earlier 99f` reaches the original text instead of refusing.
    fn node_near_save(&self, target: i64, back: bool) -> NodeIdx {
        if target <= 0 {
            return self.end_node(true);
        }
        let target = target as u64;
        let saves = self
            .nodes
            .iter()
            .enumerate()
            .filter(|&(i, _)| self.on_travel_side(i, back))
            .filter_map(|(i, n)| n.save.map(|nr| (i, nr)))
            .filter(|&(_, nr)| if back { nr <= target } else { nr >= target });
        let pick = if back {
            saves.max_by_key(|&(_, nr)| nr)
        } else {
            saves.min_by_key(|&(_, nr)| nr)
        };
        match pick {
            Some((i, _)) => i,
            // No write on that side — travel to that end of the history instead.
            None => self.end_node(back),
        }
    }

    /// The oldest (`back`) or newest node by seq — where a travel that overshoots the
    /// history lands.
    fn end_node(&self, back: bool) -> NodeIdx {
        let ends = self.nodes.iter().enumerate();
        let pick = if back {
            ends.min_by_key(|(_, n)| n.seq)
        } else {
            ends.max_by_key(|(_, n)| n.seq)
        };
        pick.map(|(i, _)| i).unwrap_or(self.cur)
    }

    /// The **leafs** of the tree — the states `:undolist` lists (vim lists the tips of
    /// each branch, not every state, so a linear history shows one row and each
    /// abandoned branch adds one). Each is `(seq, changes, time, save)`, where
    /// `changes` is the leaf's depth from the root: how many changes reach it.
    ///
    /// An uncommitted live edit is a leaf too — it is a real reachable state, and the
    /// seq it *will* take on commit is the one `:undo {N}` accepts for it — so it is
    /// woven in with the seq/time [`view`](Self::view) shows it under. It hangs off
    /// `cur`, which stops being a leaf when it had no children of its own.
    ///
    /// Ordered oldest-first by seq, matching vim's listing order.
    fn leaves(&self, tick: u64) -> Vec<UndoLeaf> {
        let pending = self.pending(tick);
        let mut out: Vec<UndoLeaf> = self
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, n)| n.children.is_empty() && !(pending && *i == self.cur))
            .map(|(i, n)| UndoLeaf {
                seq: n.seq,
                changes: self.depth_of(i),
                time: n.time,
                save: n.save,
            })
            .collect();
        if pending {
            out.push(UndoLeaf {
                seq: self.next_seq,
                changes: self.depth_of(self.cur) + 1,
                time: self.dirty_since,
                save: None,
            });
        }
        out.sort_by_key(|l| l.seq);
        out
    }

    /// How many changes reach node `idx` from the root — its depth in the tree.
    fn depth_of(&self, idx: NodeIdx) -> usize {
        let mut depth = 0;
        let mut at = idx;
        while let Some(parent) = self.nodes[at].parent {
            depth += 1;
            at = parent;
        }
        depth
    }

    /// Materialize `snap` (the live, pending state) as a new child of `cur` and move
    /// onto it. Caller guarantees `dirty` — the live buffer really has diverged from
    /// `cur`.
    ///
    /// Stamped with `dirty_since`, when the change group *began* — not the commit
    /// instant, which is the (arbitrarily later) moment the next change group starts.
    /// vim stamps its header the same way, and it is what [`view`](Self::view) already
    /// reports for the state while it is still pending: without this the same state
    /// would silently change its `undotree()` timestamp on commit, and `:earlier {N}s`
    /// would measure from when a change *ended* rather than when it was made.
    fn commit(&mut self, snap: Snapshot) {
        let now = self.dirty_since;
        let seq = self.next_seq;
        self.next_seq += 1;
        let idx = self.nodes.len();
        // The snapshot is a fresh clone of the live store, so it is in sync by
        // construction — recorded here so the re-sync in `set_cur_snapshot_cursors`
        // can skip a second clone of the very same marks.
        let snap_extmark_gen = snap.extmarks.generation();
        self.nodes.push(UndoNode {
            seq,
            parent: Some(self.cur),
            children: Vec::new(),
            time: now,
            save: None,
            snap_extmark_gen,
            snap,
        });
        self.nodes[self.cur].children.push(idx);
        self.cur = idx;
        self.dirty = false;
    }

    /// Drop the oldest states until at most `keep` remain below the root —
    /// `'undolevels'`, applied after each commit.
    ///
    /// In a *branching* tree "drop the oldest" means **re-rooting**: the root's child
    /// on the path to `cur` is promoted, and the old root goes with every other
    /// subtree hanging off it. That is vim's `u_freeheader` + `u_freebranch` — losing
    /// the oldest history loses the branches that forked from it, because nothing
    /// below a discarded state is reachable any more.
    ///
    /// `cur` is a freshly-committed leaf whenever this runs, so the path always
    /// exists; the loop still bails rather than spinning if it ever doesn't.
    fn prune(&mut self, keep: usize) {
        while self.nodes.len().saturating_sub(1) > keep {
            let Some(heir) = self.child_toward_cur(0) else {
                return;
            };
            self.reroot(heir);
        }
    }

    /// The child of `from` that lies on the path down to `cur`, by walking `cur`'s
    /// parents back up. `None` when `cur` is `from` itself (nothing below to keep).
    fn child_toward_cur(&self, from: NodeIdx) -> Option<NodeIdx> {
        let mut at = self.cur;
        loop {
            let parent = self.nodes[at].parent?;
            if parent == from {
                return Some(at);
            }
            at = parent;
        }
    }

    /// Keep only `heir`'s subtree, with `heir` as the new root: compact the node
    /// vector and remap every `parent` / `children` / `cur` index onto it.
    ///
    /// Preserves the invariant that a node's index is greater than its parent's (the
    /// order `commit` pushes in), so the root stays at index `0` — which [`view`] and
    /// [`prune`] both rely on. Seqs are *not* renumbered: a state keeps the number
    /// `:undo {N}` and `undotree()` already published for it, so travel seeks the
    /// nearest surviving seq rather than assuming density.
    ///
    /// [`view`]: Self::view
    /// [`prune`]: Self::prune
    fn reroot(&mut self, heir: NodeIdx) {
        let mut keep: Vec<NodeIdx> = Vec::new();
        let mut stack = vec![heir];
        while let Some(i) = stack.pop() {
            keep.push(i);
            stack.extend(self.nodes[i].children.iter().copied());
        }
        keep.sort_unstable();
        let mut slot = vec![usize::MAX; self.nodes.len()];
        for (new, &old) in keep.iter().enumerate() {
            slot[old] = new;
        }
        let remap = |i: NodeIdx| (slot[i] != usize::MAX).then_some(slot[i]);
        // Move the surviving nodes across rather than cloning — each carries a full
        // snapshot, which is the whole reason this pruning exists.
        let mut old: Vec<Option<UndoNode>> = std::mem::take(&mut self.nodes)
            .into_iter()
            .map(Some)
            .collect();
        self.nodes = keep
            .iter()
            .map(|&i| {
                let mut n = old[i].take().expect("each kept node is moved once");
                n.parent = n.parent.and_then(remap);
                n.children.retain(|&c| slot[c] != usize::MAX);
                n.children.iter_mut().for_each(|c| *c = slot[*c]);
                n
            })
            .collect();
        self.cur = slot[self.cur];
    }

    /// Project the tree into the shape `vim.fn.undotree()` returns. Read-only: an
    /// uncommitted live edit (`dirty`) is shown as a *virtual* current node with
    /// the seq it will receive on commit, rather than committing here — committing
    /// during a read would freeze the snapshot before later same-state mutations
    /// (extmarks, marks added outside `push_undo`) and lose them on undo.
    ///
    /// `entries` is a spine (oldest child of the root first) where each entry's
    /// `alt` carries the sibling branches forking at the same point — the
    /// recursive form neovim's `undotree()` emits, from which the visualizer's
    /// parser reconstructs parent links. Order-tolerant: the consumer re-sorts
    /// children by seq, so only the parent relationships must be faithful.
    fn view(&self, tick: u64) -> UndoTreeView {
        // The pending edit forks off `cur` and will take `next_seq` on commit.
        let pending = self
            .pending(tick)
            .then_some((self.cur, self.next_seq, self.dirty_since));
        // `save_cur` is vim's `b_u_save_nr_cur` — "the file write we are now after",
        // the base `:earlier 1f` counts back from — so it is the *ancestor* walk, not
        // this node's own `save`. Editing on top of a write keeps reporting it; only
        // stepping past the write drops it. Reporting the node's own `save` made the
        // number a visualizer reads disagree with the one
        // [`Editor::undo_travel_file`] counts from.
        let save_cur = self.save_at_or_above_cur().unwrap_or(0);
        let (seq_cur, seq_last, time_cur) = match pending {
            Some((_, seq, time)) => (seq, seq, time),
            None => {
                let c = &self.nodes[self.cur];
                (c.seq, self.seq_last(), c.time)
            }
        };
        UndoTreeView {
            seq_last,
            seq_cur,
            save_last: self.save_last,
            save_cur,
            time_cur,
            entries: self.encode_children(0, pending),
        }
    }

    /// Encode the children of `parent` as an entry chain (recursively), weaving in
    /// the virtual `pending` leaf when it forks at `parent`.
    fn encode_children(&self, parent: NodeIdx, pending: Pending) -> Vec<UndoEntry> {
        let mut sibs: Vec<SibItem> = self.nodes[parent]
            .children
            .iter()
            .map(|&c| SibItem::Real(c))
            .collect();
        if let Some((p, seq, time)) = pending {
            if p == parent {
                sibs.push(SibItem::Pending { seq, time });
            }
        }
        self.encode_sibs(&sibs, pending)
    }

    /// Encode a sibling list: the head becomes the first entry (its own descendants
    /// continue the spine after it), and the remaining siblings nest into the head
    /// entry's `alt`. A `Pending` sibling is a childless leaf.
    fn encode_sibs(&self, sibs: &[SibItem], pending: Pending) -> Vec<UndoEntry> {
        let Some((head, rest)) = sibs.split_first() else {
            return Vec::new();
        };
        let alt = self.encode_sibs(rest, pending);
        let (entry, mut tail) = match *head {
            SibItem::Real(c) => {
                let n = &self.nodes[c];
                let entry = UndoEntry {
                    seq: n.seq,
                    time: n.time,
                    save: n.save,
                    alt,
                };
                (entry, self.encode_children(c, pending))
            }
            SibItem::Pending { seq, time } => (
                UndoEntry {
                    seq,
                    time,
                    save: None,
                    alt,
                },
                Vec::new(),
            ),
        };
        let mut out = vec![entry];
        out.append(&mut tail);
        out
    }
}

/// The virtual current node `vim.fn.undotree()` shows for an uncommitted live
/// edit: `(parent, seq, time)` — the node it forks off, the seq it will take on
/// commit, and its (stable) start time. `None` when nothing is pending.
type Pending = Option<(NodeIdx, u64, i64)>;

/// One entry in a sibling list during projection: an existing node or the virtual
/// pending leaf.
enum SibItem {
    Real(NodeIdx),
    Pending { seq: u64, time: i64 },
}

/// How many undoable states `'undolevels'` keeps below the root.
///
/// vim's scale is off by one at the bottom: `-1` records no undo at all, `0` is "Vi
/// compatible: one level", and any `n > 0` is `n` levels. Zero kept states means each
/// change re-roots the tree onto itself — the live text is still the tree's current
/// state, so nothing can rewind, which is what "no undo" has to mean for the snapshot
/// to stay in sync with the buffer.
fn undo_levels_to_keep(undolevels: i64) -> usize {
    if undolevels < 0 {
        0
    } else {
        (undolevels as usize).max(1)
    }
}

/// What a `:earlier` / `:later` argument asks for — vim's three units.
enum TravelArg {
    /// `{N}` — that many states.
    States(usize),
    /// `{N}s|m|h|d`, normalized to seconds.
    Seconds(i64),
    /// `{N}f` — that many file writes.
    Writes(usize),
}

/// Parse a `:earlier` / `:later` argument: a count, optionally suffixed with a time
/// unit (`s`/`m`/`h`/`d`) or `f` for file writes. `None` for anything else, which the
/// caller turns into `E475` — never a silently-ignored argument.
fn parse_travel_arg(arg: &str) -> Option<TravelArg> {
    let digits: String = arg.chars().take_while(char::is_ascii_digit).collect();
    let n: u64 = digits.parse().ok()?;
    // An absurd count is "as far as it goes", not an error and never a wrapped
    // negative that would travel the wrong way — every conversion below saturates.
    let secs = i64::try_from(n).unwrap_or(i64::MAX);
    let count = usize::try_from(n).unwrap_or(usize::MAX);
    match &arg[digits.len()..] {
        "" => Some(TravelArg::States(count)),
        "s" => Some(TravelArg::Seconds(secs)),
        "m" => Some(TravelArg::Seconds(secs.saturating_mul(60))),
        "h" => Some(TravelArg::Seconds(secs.saturating_mul(3_600))),
        "d" => Some(TravelArg::Seconds(secs.saturating_mul(86_400))),
        "f" => Some(TravelArg::Writes(count)),
        _ => None,
    }
}

/// One row of `:undolist` — a branch tip, as [`UndoTree::leaves`] finds it.
struct UndoLeaf {
    seq: u64,
    /// Depth from the root: how many changes reach this state.
    changes: usize,
    time: i64,
    save: Option<u64>,
}

/// Render an age in `secs` seconds as vim's `:undolist` "when" column.
///
/// vim prints `"{n} seconds ago"` under 100 seconds and a wall-clock `HH:MM:SS`
/// above it. bemtvi's undo timeline is deliberately **monotonic** (seconds since
/// the editor's time base — see [`UndoNode::time`]), so there is no wall clock to
/// print; every age stays relative, extending vim's own form into larger units
/// rather than contradicting it.
fn format_ago(secs: i64) -> String {
    let (n, unit) = match secs.max(0) {
        s if s < 60 => (s, "second"),
        s if s < 3600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3600, "hour"),
        s => (s / 86_400, "day"),
    };
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {unit}{plural} ago")
}

/// The `vim.fn.undotree()` result for one buffer — neovim's dict, minus the
/// fields the visualizer ignores. Built by the core; the server serializes it.
pub struct UndoTreeView {
    pub seq_last: u64,
    pub seq_cur: u64,
    pub save_last: u64,
    pub save_cur: u64,
    pub time_cur: i64,
    pub entries: Vec<UndoEntry>,
}

/// One node in a [`UndoTreeView`]: a state plus the sibling branches forking
/// below the same parent (`alt`), recursively. Matches a neovim `undotree()`
/// entry (`seq`, `time`, optional `save`, optional `alt`).
pub struct UndoEntry {
    pub seq: u64,
    pub time: i64,
    pub save: Option<u64>,
    pub alt: Vec<UndoEntry>,
}

impl Editor {
    /// Snapshot buffer `id`'s live state. The cursor is the live cursor when `id`
    /// is current, else the buffer's saved cursor.
    fn snapshot_of(&self, id: BufferId) -> Snapshot {
        let ob = self.buffers.get(id);
        let cursor = if id == self.cur_buffer() {
            self.cursor
        } else {
            ob.saved_cursor
        };
        // Only the current (focused) buffer carries a live secondary-cursor set,
        // owned by the focused window; a background buffer's snapshot has no owning
        // window (and no `CURSOR_NS` marks).
        let cursor_window = (id == self.cur_buffer()).then_some(self.windows.current);
        Snapshot {
            text: ob.buffer.text.clone(),
            cursor,
            extmarks: ob.buffer.extmarks.clone(),
            marks: ob.buffer.marks.clone(),
            changelist: (ob.buffer.changelist.clone(), ob.buffer.changelistidx),
            cursor_window,
            selection: (id == self.cur_buffer())
                .then(|| self.undo_selection())
                .flatten(),
        }
    }

    /// Commit any pending edit on buffer `id` into its tree as a new node. A no-op
    /// when nothing is pending. Called at every change-group boundary and before
    /// any navigation, so the tree node always exists before we leave a state.
    pub(crate) fn commit_undo(&mut self, id: BufferId) {
        let ob = self.buffers.get_mut(id);
        if !ob.undo.dirty {
            return;
        }
        // A group that changed nothing closes without becoming a state.
        if !ob.undo.pending(ob.buffer.changedtick) {
            ob.undo.dirty = false;
            return;
        }
        let snap = self.snapshot_of(id);
        let keep = undo_levels_to_keep(self.buffers.get(id).buffer.options.undolevels);
        let ob = self.buffers.get_mut(id);
        ob.undo.commit(snap);
        // Bound the history to `'undolevels'`. Pruning here (rather than when the
        // option is set) matches vim: a new limit governs what is kept from the next
        // change on, it does not retroactively free what is already recorded.
        ob.undo.prune(keep);
    }

    /// Record that buffer `id` was just written to disk: commit any pending edit
    /// (so the written state is a real node), bump the save counter, and stamp the
    /// current node as that save. Drives `vim.fn.undotree()`'s `save`/`save_last`
    /// and the saved-state clean check.
    pub(crate) fn mark_undo_saved(&mut self, id: BufferId) {
        self.commit_undo(id);
        let ob = self.buffers.get_mut(id);
        ob.undo.save_last += 1;
        let nr = ob.undo.save_last;
        let cur = ob.undo.cur;
        ob.undo.nodes[cur].save = Some(nr);
        ob.saved_seq = Some(ob.undo.cur_seq());
    }

    /// The `vim.fn.undotree()` projection for buffer `id`. Read-only — a pending
    /// uncommitted edit is shown as a virtual current node (see [`UndoTree::view`])
    /// rather than committed here, so it stays visible without freezing the
    /// snapshot before later same-state extmark/mark mutations.
    pub fn undotree_of(&self, id: BufferId) -> UndoTreeView {
        let ob = self.buffers.get(id);
        ob.undo.view(ob.buffer.changedtick)
    }

    /// `:undol[ist]` — list the **leafs** in the tree of changes into a read-only
    /// scratch listing (vim lists branch tips, not every state: a linear history is
    /// one row, and each abandoned branch adds one). Columns are vim's `number`
    /// (seq), `changes` (depth from the root), `when` (see [`format_ago`]) and
    /// `saved` (the write number, blank for a state never written).
    ///
    /// Read-only, like [`Editor::undotree_of`]: an uncommitted live edit is listed as
    /// the virtual state it will become rather than committed here, so opening the
    /// listing never freezes a snapshot mid-change-group.
    pub(crate) fn ex_undolist(&mut self) {
        let id = self.cur_buffer();
        let tick = self.buffers.get(id).buffer.changedtick;
        let tree = &self.buffers.get(id).undo;
        if tree.seq_last() == 0 && !tree.pending(tick) {
            self.echo("Nothing to undo");
            return;
        }
        let now = self.now_mono;
        let mut lines = vec!["number changes  when               saved".to_string()];
        for leaf in tree.leaves(tick) {
            let mut row = format!(
                "{:>6} {:>7}  {}",
                leaf.seq,
                leaf.changes,
                format_ago(now - leaf.time)
            );
            if let Some(nr) = leaf.save {
                // vim pads the "when" column out to 33 before the saved number.
                let pad = 33usize.saturating_sub(row.chars().count());
                row.push_str(&" ".repeat(pad));
                row.push_str(&format!("  {nr:>3}"));
            }
            lines.push(row);
        }
        self.open_scratch_listing("[Undo]", lines, 0);
    }

    /// A cheap fingerprint of buffer `id`'s undo projection — `(next_seq, cur,
    /// save_last, dirty)`. The server caches it to skip rebuilding an unchanged
    /// tree's mirror on every buffer-mirror push (the hot input path). These four
    /// fully determine the projection: `next_seq` is the pending node's seq and
    /// bumps on commit, `cur` moves on undo/redo, `save_last` on write, `dirty` on
    /// edit-start (the pending node's timestamp is fixed once dirty, so it adds no
    /// drift).
    pub fn undo_version(&self, id: BufferId) -> (u64, usize, u64, bool) {
        let ob = self.buffers.get(id);
        let t = &ob.undo;
        (
            t.next_seq,
            t.cur,
            t.save_last,
            t.pending(ob.buffer.changedtick),
        )
    }

    /// Begin an independently-undoable change to buffer `id` (a workspace edit:
    /// LSP rename / code action). Finalizes any prior pending edit first, then
    /// marks the upcoming mutation pending. Unlike [`Editor::push_undo`] it does
    /// not consult `snapshot_taken` — a workspace edit is a one-shot mutation.
    pub(crate) fn push_undo_for(&mut self, id: BufferId) {
        self.commit_undo(id);
        // Bake the live cursor into the node we'll undo back to — exactly as
        // [`push_undo`](Self::push_undo) does. A rename is typically reached by
        // *navigating* to the symbol (no intervening edit), so the node `cur` is the
        // root committed at buffer load, whose frozen snapshot cursor is the top of
        // file; without this refresh, undoing the rename would jump the cursor there
        // instead of back to the symbol. Only meaningful for the focused buffer,
        // whose live cursor this reads — a background buffer's node keeps the saved
        // cursor `commit_undo` already snapshotted.
        if id == self.cur_buffer() {
            self.refresh_undo_cursor_marks(id);
        }
        let now = self.now_mono;
        let ob = self.buffers.get_mut(id);
        ob.undo.mark_dirty(now, ob.buffer.changedtick);
    }

    /// Begin a new change group on the current buffer: finalize the previous
    /// group as a node, then mark the upcoming edit pending. The `snapshot_taken`
    /// guard coalesces a multi-edit group (an insert session, a `:g` batch) into a
    /// single node — every edit between a set/clear of the flag shares one commit.
    pub(crate) fn push_undo(&mut self) {
        if self.snapshot_taken {
            // The session's group is open, but in *this* buffer's tree? `snapshot_taken`
            // is one editor-wide flag, and the tree's own group closes under it: a save
            // commits it, a reload or a switch to another buffer brings a tree with
            // none open. Edits made then would belong to no group at all, and the next
            // undo would step back past them for good. Open this buffer's group now.
            if !self.cur_mut().undo.dirty {
                let id = self.cur_buffer();
                self.refresh_undo_cursor_marks(id);
                let now = self.now_mono;
                let ob = self.cur_mut();
                ob.undo.mark_dirty(now, ob.buffer.changedtick);
            }
            return;
        }
        let id = self.cur_buffer();
        self.commit_undo(id);
        // Bake the live cursor set into the node we'll undo back to, so undoing the
        // edit we're about to make restores the cursor(s) to where they are *now* —
        // not where this edit will shift them. The node `cur` may have been
        // committed earlier (we're starting a fresh edit from a state we navigated
        // to or undid back to), so its frozen snapshot cursor would otherwise be
        // stale: undo would jump to the root's top-of-file default, or to a stale
        // multi-cursor set baked in at a branch point.
        self.refresh_undo_cursor_marks(id);
        let now = self.now_mono;
        let ob = self.cur_mut();
        ob.undo.mark_dirty(now, ob.buffer.changedtick);
    }

    /// Open a fresh typing state for the insert session that just began: no step
    /// boundary is owed until the session's own typing creates one.
    pub(crate) fn reset_typing_undo(&mut self) {
        self.typing_undo = TypingUndo {
            tick: self.buffer().changedtick,
            ..TypingUndo::default()
        };
    }

    /// Whether the keys reaching the insert path are a replay of one operation —
    /// a paste span, a dot-repeat, a macro, `:normal`, `:g` — which undoes as a
    /// single step, so typing inside it never opens a new one.
    fn replaying_one_change(&self) -> bool {
        self.paste_active
            || self.replaying_change
            || self.normal_depth > 0
            || self.in_global
            || self.macro_executing.is_some()
    }

    /// Whether typing rules stand down: a replay (see
    /// [`replaying_one_change`](Self::replaying_one_change)) or an atomic span.
    pub(super) fn typing_undo_suppressed(&self) -> bool {
        self.replaying_one_change() || self.undo_atomic > 0
    }

    /// Open a span of keys the user did not type one by one — a fired mapping's
    /// RHS, keys a plugin fed — which undoes as **one** step however much text it
    /// produces, like a paste: the span's first edit closes the typing step before
    /// it, no typing rule breaks inside it, and (when it changed the text) the
    /// typing after it starts a new step. A span that edits nothing leaves the
    /// typing around it alone. Spans nest; only the outermost one acts. The server
    /// brackets them, since only it knows which keys came from a mapping.
    pub fn begin_undo_atomic(&mut self) {
        if self.undo_atomic == 0 {
            self.undo_atomic_tick = self.buffer().changedtick;
            self.undo_atomic_opened = false;
        }
        self.undo_atomic += 1;
    }

    /// Close a span opened by [`Self::begin_undo_atomic`].
    pub fn end_undo_atomic(&mut self) {
        self.undo_atomic = self.undo_atomic.saturating_sub(1);
        if self.undo_atomic == 0 && self.buffer().changedtick != self.undo_atomic_tick {
            self.owe_typing_step();
        }
        // The span's keys were free to leave Insert (a fed `<Esc>dd`); once it
        // closes, a modeless session settles back where it rests.
        if self.undo_atomic == 0 {
            self.settle_modeless();
        }
    }

    /// Called by the insert path just before a typed key edits the text: open a
    /// new undo step when this edit should start one, so an insert session undoes
    /// a word at a time instead of all at once. A step ends at a word's end, after
    /// a line break (see [`starts_typing_step`]), where typing turns into deleting
    /// or back, after a pause longer than [`UNDO_PAUSE_MS`], and when the cursor
    /// has moved since the last typed edit. Runs once per key, before the
    /// multi-cursor sweep, so one step spans every cursor.
    pub(crate) fn before_typed_edit(&mut self, kind: EditKind, ch: Option<char>) {
        // A typed edit always lands in an open group of this buffer (see `push_undo`).
        self.push_undo();
        if self.undo_atomic > 0 {
            self.open_atomic_step();
            return;
        }
        if self.replaying_one_change() {
            return;
        }
        let t = self.typing_undo;
        let changed = self.buffer().changedtick != t.tick;
        let moved =
            t.at.is_some_and(|at| at != (self.cursor.line, self.cursor.col));
        let paused = t.kind.is_some() && self.now_ms.saturating_sub(t.last_ms) > UNDO_PAUSE_MS;
        let starts = t.force
            || moved
            || paused
            || t.kind.is_some_and(|k| k != kind)
            || (kind == EditKind::Insert
                && t.kind == Some(EditKind::Insert)
                && starts_typing_step(t.last, ch));
        if changed && starts {
            self.break_undo_step();
        }
        self.typing_undo.kind = Some(kind);
        self.typing_undo.last = ch;
        self.typing_undo.force = false;
        self.typing_undo.last_ms = self.now_ms;
    }

    /// Called by the insert path once a typed key's edit is done: remember where
    /// it left the cursor, so the next edit can tell whether the cursor moved.
    pub(crate) fn after_typed_edit(&mut self) {
        self.typing_undo.at = Some((self.cursor.line, self.cursor.col));
    }

    /// End the current typing step (when it changed anything) before something
    /// that is a step of its own lands in the insert session: a paste or register
    /// insert, an `i_CTRL-O` command. The typing that follows is owed a new step
    /// too — see [`Self::owe_typing_step`].
    pub(crate) fn end_typing_step(&mut self) {
        if self.undo_atomic > 0 {
            self.open_atomic_step();
            return;
        }
        if !self.mode.is_insert() || self.replaying_one_change() {
            return;
        }
        // What follows must land in an open group of this buffer (see `push_undo`).
        self.push_undo();
        if self.buffer().changedtick != self.typing_undo.tick {
            self.break_undo_step();
        }
    }

    /// The first edit inside an atomic span: close the typing step before it, once,
    /// so the span's text undoes on its own. Later edits in the span join it.
    fn open_atomic_step(&mut self) {
        if self.undo_atomic_opened {
            return;
        }
        self.undo_atomic_opened = true;
        if self.mode.is_insert()
            && !self.replaying_one_change()
            && self.buffer().changedtick != self.typing_undo.tick
        {
            self.break_undo_step();
        }
    }

    /// Select mode's type-over is a step of its own: close the step that is open
    /// (a snippet expansion's, whose placeholder is being typed over) before the
    /// selection is deleted, so undo puts the placeholder's default back. The
    /// deletion and the typing that replaces it then share one step.
    pub(crate) fn begin_replacing_step(&mut self) {
        if self.snapshot_taken
            && !self.typing_undo_suppressed()
            && self.buffer().changedtick != self.typing_undo.tick
        {
            self.break_undo_step();
        }
        self.reset_typing_undo();
    }

    /// Owe the next typed edit a new step: what follows a paste or register
    /// insert, an `i_CTRL-O` command or a snippet expansion never joins it.
    pub(crate) fn owe_typing_step(&mut self) {
        self.typing_undo.force = true;
    }

    /// Commit the pending step as its own node and open the next one, *without*
    /// leaving the insert session (`snapshot_taken` stays set). The cursor marks
    /// are baked into the committed node, so undoing the next step puts the cursor
    /// back where it began.
    fn break_undo_step(&mut self) {
        let id = self.cur_buffer();
        self.commit_undo(id);
        self.refresh_undo_cursor_marks(id);
        let now = self.now_mono;
        let ob = self.cur_mut();
        ob.undo.mark_dirty(now, ob.buffer.changedtick);
        self.typing_undo.tick = self.buffer().changedtick;
    }

    /// Make the live caret and selection what undo returns to, before an edit that
    /// is about to drop the selection: the typing step closes here when it changed
    /// anything, so the state it leaves is a node of its own, and otherwise the
    /// current node, whose text is still the live text, takes them. Inside a
    /// mapping's span this counts as the span's first edit; once the span has
    /// edited, its edits are one step and stay that way.
    pub(crate) fn mark_undo_point(&mut self) {
        if !self.snapshot_taken
            || self.replaying_one_change()
            || (self.undo_atomic > 0 && self.undo_atomic_opened)
        {
            return;
        }
        self.push_undo();
        if self.undo_atomic > 0 {
            self.undo_atomic_opened = true;
        }
        if self.buffer().changedtick != self.typing_undo.tick {
            self.break_undo_step();
        } else {
            let id = self.cur_buffer();
            self.refresh_undo_cursor_marks(id);
        }
    }

    /// The selection the current undo state was left with — see
    /// [`Snapshot::selection`].
    pub(crate) fn undo_state_selection(&self) -> Option<(usize, usize)> {
        let tree = &self.buffers.get(self.cur_buffer()).undo;
        tree.nodes[tree.cur].snap.selection
    }

    /// Update the current undo node's snapshot so its cursor and multi-cursor marks
    /// match the live state — see [`push_undo`](Self::push_undo). Runs for the
    /// single-cursor case too: the primary is re-synced (so undo lands at the
    /// change, not the node's stale committed cursor) and any stale [`CURSOR_NS`]
    /// marks left in the snapshot are cleared (so a single-cursor undo never
    /// resurrects a multi-cursor set the user has since collapsed).
    fn refresh_undo_cursor_marks(&mut self, id: BufferId) {
        let primary = self.cursor;
        let positions = self.secondary_cursor_bytes();
        let window = Some(self.windows.current);
        let selection = self.undo_selection();
        let ob = self.buffers.get_mut(id);
        // The extmark store is re-cloned only when a mark has actually been set /
        // deleted / cleared since this node was last synced — the structural
        // `generation`. Cloning it unconditionally is per-change-group work that
        // scales with the decoration count (5000 extmarks measured ~30% on typing),
        // which is exactly the kind of total-size-per-event cost the editor must not
        // pay. The per-file `marks` map is a handful of entries, so it always rides.
        let live_extmark_gen = ob.buffer.extmarks.generation();
        let extmarks = (ob.undo.cur_snapshot_extmark_gen() != live_extmark_gen)
            .then(|| ob.buffer.extmarks.clone());
        let marks = ob.buffer.marks.clone();
        ob.undo.set_cur_snapshot_cursors(
            primary,
            &positions,
            window,
            extmarks,
            marks,
            live_extmark_gen,
        );
        let cur = ob.undo.cur;
        ob.undo.nodes[cur].snap.selection = selection;
    }

    pub(crate) fn undo(&mut self) {
        let id = self.cur_buffer();
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        let Some(parent) = tree.nodes[tree.cur].parent else {
            self.echo("Already at oldest change");
            return;
        };
        let snap = tree.nodes[parent].snap.clone();
        let seq = tree.nodes[parent].seq;
        self.buffers.get_mut(id).undo.cur = parent;
        self.restore_snapshot(snap, seq);
    }

    pub(crate) fn redo(&mut self) {
        let id = self.cur_buffer();
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        // Plain redo follows the newest branch — the last-created child.
        let Some(child) = tree.nodes[tree.cur].children.last().copied() else {
            self.echo("Already at newest change");
            return;
        };
        let snap = tree.nodes[child].snap.clone();
        let seq = tree.nodes[child].seq;
        self.buffers.get_mut(id).undo.cur = child;
        self.restore_snapshot(snap, seq);
    }

    /// `:undo {N}` — jump the current buffer to the state with seq `N`, anywhere
    /// in the tree (including an abandoned branch a stack-based undo could never
    /// reach). `:undo 0` returns to the original loaded text.
    pub(crate) fn undo_to_seq(&mut self, target: u64) {
        let id = self.cur_buffer();
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        let Some(node) = tree.node_of_seq(target) else {
            self.echo(format!("E830: Undo number {target} not found"));
            return;
        };
        if node == tree.cur {
            return;
        }
        let snap = tree.nodes[node].snap.clone();
        self.buffers.get_mut(id).undo.cur = node;
        self.restore_snapshot(snap, target);
    }

    /// `g-` / `g+` / `:earlier {N}` / `:later {N}` — move `count` states along **seq
    /// order**, across branches, going back when `back`.
    ///
    /// This is deliberately not `u`/`<C-r>`: those walk the tree (to the parent, to
    /// the newest child), while this walks the states in the order they were made.
    /// On a linear history the two coincide; once a branch exists they do not, which
    /// is the whole point of the pair — `g-` reaches an abandoned branch's states
    /// that no amount of `u` can.
    ///
    /// The target is clamped to the tree's ends, so a large count travels as far as
    /// it can rather than refusing; only a step that cannot move at all reports
    /// vim's boundary message.
    pub(crate) fn undo_travel(&mut self, count: usize, back: bool) {
        if count == 0 {
            return;
        }
        let id = self.cur_buffer();
        // Any pending edit becomes a real state first — otherwise the change just
        // typed would be skipped over instead of being the thing `g-` steps off.
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        let cur = tree.cur_seq() as i64;
        let step = i64::try_from(count).unwrap_or(i64::MAX);
        let target = if back {
            cur.saturating_sub(step)
        } else {
            cur.saturating_add(step)
        };
        let target = target.clamp(0, tree.seq_last() as i64) as u64;
        let node = tree.node_near_seq(target, back);
        self.undo_land_on(node, back);
    }

    /// `:earlier {N}s|m|h|d` / `:later {N}s|m|h|d` — travel to the state as it was
    /// `secs` seconds either side of **the current state's** timestamp (vim measures
    /// from `b_u_time_cur`, not from now, so repeated `:earlier 10s` keeps stepping
    /// back rather than sticking).
    pub(crate) fn undo_travel_time(&mut self, secs: i64, back: bool) {
        let id = self.cur_buffer();
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        let base = tree.nodes[tree.cur].time;
        let target = if back {
            base.saturating_sub(secs)
        } else {
            base.saturating_add(secs)
        };
        let node = tree.node_near_time(target, back);
        self.undo_land_on(node, back);
    }

    /// `:earlier {N}f` / `:later {N}f` — travel `count` **file writes**. When the
    /// current state is not itself a write, going back spends its first step reaching
    /// the last write (vim's behavior), so `:earlier 1f` from a dirty buffer returns
    /// to what is on disk.
    pub(crate) fn undo_travel_file(&mut self, count: usize, back: bool) {
        if count == 0 {
            return;
        }
        let id = self.cur_buffer();
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        let at_save = tree.nodes[tree.cur].save.is_some();
        // The write this state descends from — an *ancestor* walk, not a seq
        // comparison: a write on an abandoned branch is not behind us.
        let base = tree.save_at_or_above_cur().unwrap_or(0) as i64;
        let step = i64::try_from(count).unwrap_or(i64::MAX);
        let target = if back {
            // Not at a write? The first step is spent reaching the last one.
            base.saturating_sub(if at_save { step } else { step - 1 })
        } else {
            base.saturating_add(step)
        };
        let node = tree.node_near_save(target, back);
        self.undo_land_on(node, back);
    }

    /// Land on `node`, or report vim's boundary message when the travel could not
    /// move at all. Shared by every `g-`/`g+`/`:earlier`/`:later` form.
    fn undo_land_on(&mut self, node: NodeIdx, back: bool) {
        let id = self.cur_buffer();
        let tree = &self.buffers.get(id).undo;
        if node == tree.cur {
            self.echo(if back {
                "Already at oldest change"
            } else {
                "Already at newest change"
            });
            return;
        }
        let snap = tree.nodes[node].snap.clone();
        let seq = tree.nodes[node].seq;
        self.buffers.get_mut(id).undo.cur = node;
        self.restore_snapshot(snap, seq);
    }

    /// `:ea[rlier] [N][s|m|h|d|f]` / `:lat[er] …` — the ex form of `g-` / `g+`, with
    /// vim's time and file-write units. A bare command means one state. An argument
    /// that parses as none of these is `E475`, loud, rather than a silent no-op.
    pub(crate) fn ex_undo_travel(&mut self, args: &str, back: bool) {
        let arg = args.trim();
        if arg.is_empty() {
            self.undo_travel(1, back);
            return;
        }
        let Some(spec) = parse_travel_arg(arg) else {
            self.echo(format!("E475: Invalid argument: {arg}"));
            return;
        };
        match spec {
            TravelArg::States(n) => self.undo_travel(n, back),
            TravelArg::Seconds(s) => self.undo_travel_time(s, back),
            TravelArg::Writes(n) => self.undo_travel_file(n, back),
        }
    }

    /// Restore the current buffer to `snap` (the state numbered `seq`): swap in the
    /// text, cursor, extmarks and marks, and recompute `modified`.
    fn restore_snapshot(&mut self, mut snap: Snapshot, seq: u64) {
        // Undo restores where marks sit, not which marks exist (see
        // `ExtmarkStore::adopt_positions_from`): keep the LIVE store, taken before
        // `mark_resync` clears it, and move each mark back to its position in the
        // snapshot. Two kinds are left out of that. Decoration-provider marks
        // (`btv.decor`) are ephemeral viewport state, republished off-tick on every
        // viewport/edit change, so they keep their live positions. The multi-cursor
        // marks are history: the cursor set rides back as captured, below.
        use crate::extmark::{ANCHOR_NS, CURSOR_NS};
        let mut skip = self.ephemeral_extmark_namespaces();
        skip.extend([CURSOR_NS, ANCHOR_NS]);
        let mut extmarks = std::mem::take(&mut self.buffer_mut().extmarks);
        extmarks.adopt_positions_from(&snap.extmarks, snap.text.len(), &skip);
        for ns in [CURSOR_NS, ANCHOR_NS] {
            snap.extmarks.move_namespace_into(ns, &mut extmarks);
        }
        let ob = self.cur_mut();
        let old_text = std::mem::replace(&mut ob.buffer.text, snap.text);
        // We're back on a previously-seen state: it's clean only if it's the one
        // last written to disk. (`mark_resync` below sets `modified = true`, so
        // decide this first and re-assert it afterwards.)
        let clean = ob.saved_seq == Some(seq);
        self.cursor = snap.cursor;
        self.buffer_mut().mark_resync();
        // `mark_resync` clears extmarks and `a`–`z` marks (correct for a destructive
        // reload); undo is not a reload, so put them back: the extmarks merged above,
        // and the `a`–`z` marks and the cursor as captured with this history point.
        self.buffer_mut().extmarks = extmarks;
        self.buffer_mut().marks = snap.marks;
        let (changelist, changelistidx) = snap.changelist;
        self.buffer_mut().changelist = changelist;
        self.buffer_mut().changelistidx = changelistidx;
        self.buffer_mut().modified = !clean;
        // vim's `u_undoredo` leaves `` `[ `` / `` `] `` in column 0 of the first and
        // last lines the undo (or redo) put back.
        let (first, last) = changed_line_span(&old_text, &self.buffer().text);
        let top = self.buffer().line_count().saturating_sub(1);
        self.set_change_marks((first.min(top), 0), (last.min(top), 0));
        // The secondary multi-cursor set is window-local, but the undo tree is
        // shared by every window onto this buffer. Keep the baked `CURSOR_NS`/
        // `ANCHOR_NS` marks only when *this* window baked them; otherwise drop them
        // so undoing another window's multi-cursor edit reverts its text without
        // resurrecting its cursors here (the primary still rides back, vim-style).
        if snap.cursor_window != Some(self.windows.current) {
            let buf = self.buffer_mut();
            buf.extmarks.clear(crate::extmark::CURSOR_NS, None);
            buf.extmarks.clear(crate::extmark::ANCHOR_NS, None);
        }
        self.clamp_cursor();
        // A selection still open (Helix select, where `u` is undo) keeps its
        // anchor; the restored text may no longer reach it.
        let line = self.visual_anchor.line.min(self.last_line());
        let s = self.buffer().line(line);
        let mut col = self.visual_anchor.col.min(s.len());
        while !s.is_char_boundary(col) {
            col -= 1;
        }
        self.visual_anchor = Cursor { line, col };
    }
}

/// The lines of `new` that differ from `old`: the first line past their common
/// head through the last line before their common tail. A span that put back no
/// line at all (lines were only removed) is the line after the removal.
fn changed_line_span(old: &ropey::Rope, new: &ropey::Rope) -> (usize, usize) {
    use ropey::LineType::LF_CR as LT;
    let (on, nn) = (old.len_lines(LT), new.len_lines(LT));
    let mut head = 0;
    while head < on && head < nn && old.line(head, LT) == new.line(head, LT) {
        head += 1;
    }
    let mut tail = 0;
    while tail < on - head
        && tail < nn - head
        && old.line(on - 1 - tail, LT) == new.line(nn - 1 - tail, LT)
    {
        tail += 1;
    }
    let put_back = nn - head - tail;
    (head, head + put_back.saturating_sub(1))
}
