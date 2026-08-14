//! A cell-accurate model of a real terminal screen, for the multi-frame paint
//! harness ([`crate::paint_frames`]).
//!
//! ratatui's own `TestBackend` records the cells a frame emits into a plain grid,
//! which is faithful right up until a **double-width** glyph is involved. A real
//! terminal does not store such a glyph as one cell: it stores a head cell plus a
//! continuation cell painted with the head's colours, and — the part that catches
//! bugs — a later write landing on either half *breaks the pair*, blanking the
//! orphaned half to the terminal's own default attributes. A column the client
//! never repaints therefore shows as a hole in the background rather than keeping
//! whatever was last drawn there, so the harness that guards against that has to
//! model it. (tmux is the terminal that makes it most visible; a bare terminal may
//! paper over the same missing repaint.)
//!
//! Only the surface [`ratatui::Terminal`] actually drives is modelled; anything
//! else fails loud rather than quietly pretending.

use std::io;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Rect, Size};
use unicode_width::UnicodeWidthStr;

/// The screen a sequence of frames leaves behind.
#[derive(Debug, Clone)]
pub(crate) struct TerminalModel {
    screen: Buffer,
    cursor: bool,
    pos: Position,
}

impl TerminalModel {
    pub(crate) fn new(width: u16, height: u16) -> Self {
        Self {
            screen: Buffer::empty(Rect::new(0, 0, width, height)),
            cursor: false,
            pos: Position::new(0, 0),
        }
    }

    /// The modelled screen: what a user would be looking at.
    pub(crate) fn screen(&self) -> &Buffer {
        &self.screen
    }

    /// Is the cell at `(x, y)` the head of a double-width glyph — i.e. does it
    /// also own the column to its right?
    fn is_wide(&self, x: u16, y: u16) -> bool {
        self.screen
            .cell(Position::new(x, y))
            .is_some_and(|c| c.symbol().width() > 1)
    }

    /// Erase a cell the way a terminal erases the orphaned half of a glyph it just
    /// broke: a blank in the *default* attributes, not in the colours that were
    /// there.
    fn blank(&mut self, x: u16, y: u16) {
        if let Some(cell) = self.screen.cell_mut(Position::new(x, y)) {
            *cell = Cell::EMPTY;
        }
    }

    fn write(&mut self, x: u16, y: u16, cell: Cell) {
        if let Some(slot) = self.screen.cell_mut(Position::new(x, y)) {
            *slot = cell;
        }
    }

    /// Print one cell at `(x, y)`, breaking any glyph pair the write lands on.
    fn print(&mut self, x: u16, y: u16, cell: Cell) {
        let wide = cell.symbol().width() > 1;
        // Landing on the right half of the glyph to the left orphans its head.
        if x > 0 && self.is_wide(x - 1, y) {
            self.blank(x - 1, y);
        }
        // Overwriting a head with something narrow orphans its continuation.
        if self.is_wide(x, y) && !wide {
            self.blank(x + 1, y);
        }
        if wide {
            // The glyph claims the next column too, so a head sitting there loses
            // its own continuation.
            if self.is_wide(x + 1, y) {
                self.blank(x + 2, y);
            }
            // The continuation carries the head's colours but no symbol of its
            // own — the head's glyph is what is drawn across both columns.
            let mut continuation = cell.clone();
            continuation.set_symbol("");
            self.write(x + 1, y, continuation);
        }
        self.write(x, y, cell);
    }
}

impl Backend for TerminalModel {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            self.print(x, y, cell.clone());
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.cursor = false;
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.cursor = true;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.pos)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.pos = position.into();
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.screen.reset();
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        match clear_type {
            ClearType::All => self.screen.reset(),
            other => unimplemented!("TerminalModel does not model clear_region({other:?})"),
        }
        Ok(())
    }

    fn size(&self) -> io::Result<Size> {
        Ok(self.screen.area.as_size())
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.screen.area.as_size(),
            pixels: Size::new(0, 0),
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
