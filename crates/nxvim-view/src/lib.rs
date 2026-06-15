//! Frontend-agnostic client model for nxvim.
//!
//! Every UI is a client of the server's own RPC: it sends keystrokes as vim
//! key-notation and renders the [`View`] the server pushes in each `redraw`
//! notification (see the *View protocol* in `docs/architecture.md`). The parts of
//! that contract that don't depend on a rendering toolkit live here, so the
//! terminal client (`nxvim-tui`) and a native GUI client share them rather than
//! re-implementing the wire decode and input encoding:
//!
//! - [`View`] and its sub-models ([`WindowView`], [`Separator`], [`PanelData`],
//!   [`PmenuData`], [`TabData`], [`ScrollData`]) plus the redraw decoder
//!   ([`View::from_redraw`] / [`View::update`]).
//! - A neutral [`Style`] / [`Border`] each client converts to its own toolkit.
//! - Input encoding: the [`Key`] enum, [`notation`], and [`encode_paste`].

mod hit_test;
mod keys;
mod parse;
mod style;
mod view;

pub use hit_test::{resize_handle_at, Geometry, ResizeCursor};
pub use keys::{encode_paste, notation, Key};
pub use parse::{
    DiagSign, DiagSpan, DiagVirt, HlSpan, IncSearchSpans, InlayHint, PmenuItem, SearchSpans,
    StatusSegment,
};
pub use style::{Border, Style};
pub use view::{
    MenuData, MenuDocs, MenuPreview, PanelData, PmenuData, RegionTabline, RegionTablines,
    ScrollData, Separator, TabData, View, WinRect, WindowRegion, WindowView,
};
