//! The change list — vim's per-buffer history of the positions you *changed*,
//! navigated with `g;` (older) / `g,` (newer) and listed with `:changes`. Its head
//! is the `` `. `` last-change mark.
//!
//! Unlike the jumplist (per-window, in [`jumps`](super::jumps)), the change list
//! lives on the [`Buffer`](crate::buffer::Buffer): it is per-buffer and follows the
//! text. Recording, line-shifting on later edits, the coalesce-by-line rule, the
//! 100-entry cap, and undo snapshot/restore all live there (see `buffer.rs`); this
//! module is only the *navigation* — stepping the buffer's `changelistidx` pointer
//! and landing the cursor, plus the `:changes` listing.
//!
//! `g;` from the present (`idx == len`) lands on the newest change, skipping it
//! when it is the line the cursor already sits on (you don't "go back" to the
//! change you just made) — vim's behavior. Boundaries report `E662`/`E663` like
//! vim rather than moving silently.

use super::*;

impl Editor {
    /// `g;` — jump to an older change, `count` steps back.
    pub(crate) fn change_older(&mut self, count: usize) {
        let count = count.max(1);
        let len = self.buffer().changelist.len();
        if len == 0 {
            self.echo("E664: changelist is empty");
            return;
        }
        let idx = self.buffer().changelistidx;
        if idx != len && idx == 0 {
            self.echo("E662: At start of changelist");
            return;
        }
        // The first step from the present lands on the newest change, skipping it
        // when it is the cursor's own line (the change just made).
        let first = if idx == len {
            let newest = len - 1;
            if self.buffer().changelist[newest].0 == self.cursor.line && newest > 0 {
                newest - 1
            } else {
                newest
            }
        } else {
            idx - 1
        };
        // Remaining steps go strictly older, clamped to the oldest entry.
        let target = first.saturating_sub(count - 1);
        self.goto_change(target);
    }

    /// `g,` — jump to a newer change, `count` steps forward.
    pub(crate) fn change_newer(&mut self, count: usize) {
        let count = count.max(1);
        let len = self.buffer().changelist.len();
        if len == 0 {
            self.echo("E664: changelist is empty");
            return;
        }
        let idx = self.buffer().changelistidx;
        // At (or past) the newest change there is nothing newer to reach.
        if idx >= len.saturating_sub(1) {
            self.echo("E663: At end of changelist");
            return;
        }
        let target = (idx + count).min(len - 1);
        self.goto_change(target);
    }

    /// Set the change-list pointer to `target` and land the cursor on that change's
    /// exact `(line, col)`, clamped to the buffer. `g;`/`g,` are change-list
    /// navigation, not jumps, so this does *not* touch the jumplist.
    fn goto_change(&mut self, target: usize) {
        let (line, col) = self.buffer().changelist[target];
        self.buffer_mut().changelistidx = target;
        self.settle_cursor_at(line, col);
    }

    /// `:changes` — list the current buffer's change list into a read-only scratch listing,
    /// mirroring vim's `change line  col text` table. Entries run oldest-first; the
    /// `change` column is the count of `g;` (above the marker) / `g,` (below) presses
    /// to reach that row, and `>` marks the current position.
    pub(crate) fn ex_changes(&mut self, _args: &str) {
        let idx = self.buffer().changelistidx;
        let entries = self.buffer().changelist.clone();

        let mut lines = vec![" change line  col text".to_string()];
        for (i, &(line, col)) in entries.iter().enumerate() {
            let marker = if i == idx { '>' } else { ' ' };
            let count = if i == idx {
                String::new()
            } else {
                (idx as isize - i as isize).abs().to_string()
            };
            let text = self.buffer().line(line.min(self.last_line()));
            lines.push(format!(
                "{marker}{count:>3} {:>4} {:>4} {}",
                line + 1,
                col,
                text.trim_end()
            ));
        }
        if idx >= entries.len() {
            lines.push(">".to_string());
        }
        self.open_scratch_listing("[Changes]", lines, 0);
    }
}
