//! The viewport-changed signal for `nx.decor` — viewport-scoped decoration
//! providers (build-order step 5 of the native plugin API;
//! `docs/specs/2026-06-11-native-plugin-api.md` §6,
//! `docs/plans/2026-06-15-nx-decor-viewport-decorations.md`).
//!
//! A `nx.decor` provider is woken **once per visible-range change** (scroll,
//! resize, edit reflow), *off the frame path*, handed a snapshot of the visible
//! slice, and publishes marks carrying a **generation token**; a publish from a
//! viewport the user already scrolled past is dropped. neovim's frame-time
//! `on_win`/`on_line` model can't host that on the PUC backend (ADR 0002 rule 4),
//! so the trigger is detached from rendering: core detects "the visible range of
//! window *W* changed" when input settles, stamps a fresh generation for *W*, and
//! queues a dirty entry the server drains off-tick (in `run_pending`). That
//! detached, generation-stamped signal is the one net-new primitive — the publish
//! path reuses the extmark layer, and the dispatch/debounce reuses the
//! `nx.complete`/`nx.picker` off-tick generation machinery.
//!
//! This module is pure and synchronous (it lives in `nxvim-core`); the Lua
//! provider dispatch and the publish→extmark lowering live in `nxvim-server`.

use super::*;

/// A window whose visible range changed since the last decor recompute — the unit
/// the server drains and dispatches to matching providers. `top`/`bot` are 0-based
/// inclusive buffer rows; `generation` is the window's viewport generation at
/// detection, which a resulting publish carries back so a superseded viewport
/// drops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecorViewport {
    pub win: WindowId,
    pub buf: BufferId,
    pub top: usize,
    pub bot: usize,
    pub generation: u64,
}

/// Which windows an [`Editor::invalidate_decor`] applies to — everything, every
/// window showing one buffer, or a single window. The `nx.decor.invalidate` scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecorScope {
    /// Every visible window (an unscoped `nx.decor.invalidate()`).
    All,
    /// Every window currently displaying this buffer — the usual scope, since a
    /// provider's data is per-buffer and the same buffer can be in several splits.
    Buffer(BufferId),
    /// One window (a provider that knows exactly which viewport went stale).
    Window(WindowId),
}

impl Editor {
    /// Detect every visible (tiled) window whose `(buffer, top, bot)` changed since
    /// the last call, bump that window's viewport generation, and queue a
    /// [`DecorViewport`] for the server to dispatch. Called when input settles
    /// (tail of [`Editor::input`]), after a resize, and once more by the server right
    /// before it drains the dirty list in `run_pending` — so a viewport change driven
    /// *off* the input tick (a `:e` run from a queued command-line action, a buffer
    /// switch from a Lua callback) is still detected, rather than waiting for the next
    /// keystroke. *Not* from the redraw projection, so the Lua provider never runs
    /// during a frame.
    ///
    /// Latest-wins per window: a window already pending in `decor_dirty` is replaced
    /// (held `<C-e>` between two drains collapses to one provider run — Decision 2).
    pub fn recompute_decor_dirty(&mut self) {
        let mut seen: HashSet<WindowId> = HashSet::new();
        for win in self.window_ids() {
            seen.insert(win);
            let Some(buf) = self.window_buffer(win) else {
                continue;
            };
            let top = self.window_top(win);
            let height = self.window_text_area(win).map_or(0, |(_, h)| h);
            let (last_line, tick) = self.buffer_of(buf).map_or((0, 0), |b| {
                (b.line_count().saturating_sub(1), b.changedtick)
            });
            let bot = top.saturating_add(height.saturating_sub(1)).min(last_line);
            // `changedtick` is in the key so an on-screen edit that leaves top/bot
            // unchanged (typing within the viewport) still re-dispatches the provider.
            let key = (buf, top, bot, tick);
            let moved = self.decor_viewports.get(&win) != Some(&key);
            // An outstanding `nx.decor.invalidate` re-dispatches this window even
            // though the viewport itself is unchanged — but at most ONCE per pass.
            // The ask is never refused: it is paced, the same way `decor_dirty` is
            // latest-wins and a superseded publish is dropped. What that buys is a
            // loop that cannot run away — a provider whose `on_range` asks to be run
            // again (directly, or from a continuation that lands before the fixpoint
            // settles) is answered once, and the ask its *second* run raises waits for
            // the next pass instead of spinning the convergence. Nothing is lost: the
            // ask stays outstanding until it is served.
            let serve = self.decor_invalidated_wins.contains(&win)
                && !self.decor_served_wins.contains(&win);
            if !moved && !serve {
                continue;
            }
            // Either way the window is about to be re-dispatched with fresh state, so
            // any outstanding ask for it is satisfied. A real viewport change answers
            // an invalidation for free, and never spends the pass's slot.
            self.decor_invalidated_wins.remove(&win);
            if !moved {
                self.decor_served_wins.insert(win);
            }
            self.decor_viewports.insert(win, key);
            let counter = self.decor_gen.entry(win).or_insert(0);
            *counter += 1;
            let generation = *counter;
            self.decor_dirty.retain(|d| d.win != win);
            self.decor_dirty.push(DecorViewport {
                win,
                buf,
                top,
                bot,
                generation,
            });
        }
        // Forget windows that have since closed so their generations don't leak (a
        // late publish for a closed window then fails the gen check and is dropped).
        self.decor_viewports.retain(|w, _| seen.contains(w));
        self.decor_gen.retain(|w, _| seen.contains(w));
        self.decor_dirty.retain(|d| seen.contains(&d.win));
        // …including an invalidation naming a window that has since closed (or never
        // existed): it can never be served, so it must not accumulate.
        self.decor_invalidated_wins.retain(|w| seen.contains(w));
        self.decor_served_wins.retain(|w| seen.contains(w));
    }

    /// Invalidate the cached viewport of the windows `scope` selects, so the next
    /// [`Editor::recompute_decor_dirty`] re-queues them with a **fresh generation**
    /// and their providers run again — the "there is new content to draw" signal a
    /// plugin raises through `nx.decor.invalidate` when nothing the viewport detector
    /// watches has moved.
    ///
    /// The viewport signal only wakes a provider when `(buffer, top, bot,
    /// changedtick)` changes, which covers scroll / resize / edit but *not* a change
    /// in the data the provider draws *from*: git blame that just came back off a
    /// promise, an LSP result, a toggled setting. Rather than let a plugin fake a
    /// viewport move (or re-derive the marks and push them behind the engine's back),
    /// this marks the windows at the layer that owns the signal — so the re-dispatch
    /// is the ordinary one, gen-stamped like any other, and a publish still in flight
    /// from the superseded run is dropped by the same staleness check.
    ///
    /// The scope is resolved to window ids *here*, while the layout is in hand, and
    /// the generation bump is left to `recompute_decor_dirty` so one place stamps
    /// generations. Marking the windows (rather than dropping their cached key) is
    /// what lets the recompute tell an invalidation-driven re-dispatch apart from a
    /// real viewport move — the distinction its once-per-pass pacing rests on. The ask
    /// stays outstanding until it is served, so an invalidation is never dropped, and
    /// repeated asks for the same window coalesce into one re-dispatch.
    pub fn invalidate_decor(&mut self, scope: DecorScope) {
        match scope {
            DecorScope::All => {
                let wins = self.window_ids();
                self.decor_invalidated_wins.extend(wins);
            }
            DecorScope::Window(win) => {
                self.decor_invalidated_wins.insert(win);
            }
            DecorScope::Buffer(buf) => {
                let wins: Vec<WindowId> = self
                    .window_ids()
                    .into_iter()
                    .filter(|w| self.window_buffer(*w) == Some(buf))
                    .collect();
                self.decor_invalidated_wins.extend(wins);
            }
        }
        self.decor_invalidated = true;
    }

    /// Close the current pass: the per-pass invalidation slots are spent per window,
    /// so clear them once the server's `run_pending` fixpoint has settled. Pacing is
    /// per convergence and nothing else — a plugin that invalidates from a timer (a
    /// clock or blame line refreshing every few seconds) lands in its own pass and is
    /// served every time; only re-asking *within* one pass waits for the next.
    pub fn settle_decor_pass(&mut self) {
        self.decor_served_wins.clear();
    }

    /// Whether an invalidation has been raised but not yet drained — the server's
    /// cue that its fixpoint owes another round (an invalidate from inside a provider,
    /// or from a promise continuation, lands after the dispatch step has already run
    /// for this round).
    pub fn decor_invalidation_pending(&self) -> bool {
        self.decor_invalidated
    }

    /// Drain the windows whose viewport changed since the last drain (the server
    /// dispatches each to matching providers off-tick). Also consumes the
    /// invalidation flag: this is the single drain point (with or without a
    /// registered provider), so clearing it here is what keeps an invalidate from
    /// spinning the server's fixpoint.
    pub fn take_decor_dirty(&mut self) -> Vec<DecorViewport> {
        self.decor_invalidated = false;
        std::mem::take(&mut self.decor_dirty)
    }

    /// Window `win`'s current viewport generation — the live value a publish's
    /// stamped generation is checked against before it is applied (a stale publish,
    /// from a viewport the user scrolled past, is dropped). `0` for an unknown or
    /// closed window, which no live publish can match.
    pub fn decor_generation(&self, win: WindowId) -> u64 {
        self.decor_gen.get(&win).copied().unwrap_or(0)
    }

    /// Record that extmark namespace `ns` holds **ephemeral** decoration-provider
    /// marks (republished off-tick on every viewport/edit change), so undo/redo carry
    /// the live marks across a snapshot restore rather than swapping them out — see
    /// [`Editor::restore_snapshot`] and the `ephemeral_extmark_ns` field. Idempotent;
    /// the server calls it when a `nx.decor` publish first targets a namespace.
    pub fn mark_extmark_namespace_ephemeral(&mut self, ns: u32) {
        self.ephemeral_extmark_ns.insert(ns);
    }

    /// The set of registered ephemeral namespaces (decoration-provider publishes),
    /// for the undo restore to carry their live marks across a snapshot swap.
    pub(crate) fn ephemeral_extmark_namespaces(&self) -> Vec<u32> {
        self.ephemeral_extmark_ns.iter().copied().collect()
    }
}
