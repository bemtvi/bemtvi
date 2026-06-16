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

impl Editor {
    /// Detect every visible (tiled) window whose `(buffer, top, bot)` changed since
    /// the last call, bump that window's viewport generation, and queue a
    /// [`DecorViewport`] for the server to dispatch. Called when input settles
    /// (tail of [`Editor::input`]) and after a resize — *not* from the redraw
    /// projection, so the Lua provider never runs during a frame.
    ///
    /// Latest-wins per window: a window already pending in `decor_dirty` is replaced
    /// (held `<C-e>` between two drains collapses to one provider run — Decision 2).
    pub(crate) fn recompute_decor_dirty(&mut self) {
        let mut seen: HashSet<WindowId> = HashSet::new();
        for win in self.window_ids() {
            seen.insert(win);
            let Some(buf) = self.window_buffer(win) else {
                continue;
            };
            let top = self.window_top(win);
            let height = self.window_content_size(win).map_or(0, |(_, h)| h);
            let last_line = self
                .buffer_of(buf)
                .map_or(0, |b| b.line_count().saturating_sub(1));
            let bot = top.saturating_add(height.saturating_sub(1)).min(last_line);
            let key = (buf, top, bot);
            if self.decor_viewports.get(&win) == Some(&key) {
                continue;
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
    }

    /// Drain the windows whose viewport changed since the last drain (the server
    /// dispatches each to matching providers off-tick).
    pub fn take_decor_dirty(&mut self) -> Vec<DecorViewport> {
        std::mem::take(&mut self.decor_dirty)
    }

    /// Window `win`'s current viewport generation — the live value a publish's
    /// stamped generation is checked against before it is applied (a stale publish,
    /// from a viewport the user scrolled past, is dropped). `0` for an unknown or
    /// closed window, which no live publish can match.
    pub fn decor_generation(&self, win: WindowId) -> u64 {
        self.decor_gen.get(&win).copied().unwrap_or(0)
    }
}
