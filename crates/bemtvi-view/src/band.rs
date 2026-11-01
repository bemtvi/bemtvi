//! One band row under a horizontal scroll, for the frames of a slide.
//!
//! A settled window arrives clipped: the server applies `leftcol` to its rows' inline
//! cells and sends `row_leftcol`. A slide's column changes every frame between the two
//! ends the server knows, so the client applies the interpolated one to the band's
//! unclipped payload here. The model is the server's (`bemtvi_core::unicode::row_scroll`):
//! `leftcol` counts text and inline cells together, so the last frame of a slide paints
//! what the settled window does.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{InlayHint, VirtChunk, VirtPlacement};

/// Placement `pos` of inline `virt_text` on the wire.
const POS_INLINE: u8 = 1;

/// A band row's inline cells once a scroll has reached them.
pub struct BandRow {
    /// The text column the row paints from.
    pub left: u16,
    /// The hints still showing, the one the scroll falls inside trimmed.
    pub inlay: Vec<InlayHint>,
    /// The placements still showing, likewise. Other positions pass through.
    pub vtext: Vec<VirtPlacement>,
}

fn cells(s: &str) -> usize {
    s.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// `s` without its first `n` cells; a wide character cut in half leaves a blank.
fn skip_cells(s: &str, n: usize) -> String {
    let mut left = n;
    let mut out = String::new();
    for g in s.graphemes(true) {
        let w = UnicodeWidthStr::width(g);
        if left == 0 {
            out.push_str(g);
        } else if left >= w {
            left -= w;
        } else {
            out.extend(std::iter::repeat_n(' ', w - left));
            left = 0;
        }
    }
    out
}

fn skip_chunks(chunks: &[VirtChunk], mut n: usize) -> Vec<VirtChunk> {
    let mut out = Vec::with_capacity(chunks.len());
    for (text, id) in chunks {
        let w = cells(text);
        if n >= w {
            n -= w;
        } else {
            out.push((skip_cells(text, n), *id));
            n = 0;
        }
    }
    out
}

/// Apply column `leftcol` to a band row's `inlay` hints and `vtext` placements. `None`
/// when it reaches none of their cells, the common case: the row paints from `leftcol`
/// as it is, and a frame allocates nothing for it.
pub fn clip_band_row(
    leftcol: u16,
    inlay: &[InlayHint],
    vtext: &[VirtPlacement],
) -> Option<BandRow> {
    let leftcol = leftcol as usize;
    // Paint order: by anchor, an inlay hint before inline text at a shared anchor.
    let mut items: Vec<(usize, usize, Result<usize, usize>)> = inlay
        .iter()
        .enumerate()
        .map(|(i, h)| (h.0 as usize, cells(&h.1), Ok(i)))
        .chain(
            vtext
                .iter()
                .enumerate()
                .filter(|(_, p)| p.pos == POS_INLINE)
                .map(|(i, p)| {
                    let width = p.chunks.iter().map(|c| cells(&c.0)).sum();
                    (p.col as usize, width, Err(i))
                }),
        )
        .collect();
    items.sort_by_key(|item| item.0);

    let mut shift = 0;
    let mut hidden = items.len();
    let mut skip = 0;
    for (i, &(anchor, width, _)) in items.iter().enumerate() {
        let start = anchor + shift;
        if leftcol <= start {
            hidden = i;
            break;
        }
        if leftcol < start + width {
            hidden = i;
            skip = leftcol - start;
            break;
        }
        shift += width;
    }
    if hidden == 0 && skip == 0 {
        return None;
    }
    let gone = |which: Result<usize, usize>| items[..hidden].iter().any(|item| item.2 == which);
    let trimmed = (skip > 0).then(|| items[hidden].2);
    let inlay = inlay
        .iter()
        .enumerate()
        .filter(|(i, _)| !gone(Ok(*i)))
        .map(|(i, h)| {
            let mut h = h.clone();
            if trimmed == Some(Ok(i)) {
                h.1 = skip_cells(&h.1, skip);
            }
            h
        })
        .collect();
    let vtext = vtext
        .iter()
        .enumerate()
        .filter(|(i, _)| !gone(Err(*i)))
        .map(|(i, p)| {
            let mut p = p.clone();
            if trimmed == Some(Err(i)) {
                p.chunks = skip_chunks(&p.chunks, skip);
            }
            p
        })
        .collect();
    Some(BandRow {
        left: (leftcol - shift - skip) as u16,
        inlay,
        vtext,
    })
}
