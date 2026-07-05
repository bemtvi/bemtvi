//! The GPU renderer: a wgpu surface, a tiny solid-color quad pipeline (for
//! backgrounds, the visual selection, search matches, per-window status bars and
//! the block/bar cursor), and a [glyphon] text layer on top.
//!
//! It is the GUI analogue of `nxvim-tui`'s `render` module: it projects the
//! server's [`View`] — the same `redraw` model every client consumes — into
//! pixels. The server already resolved every highlight group to a concrete
//! [`Style`]; this renderer just paints truecolor. Layout is a fixed monospace
//! **cell grid**: the cell size is measured once from the font, and window rects,
//! cursor, and spans (all in cells/screen-columns on the wire) are multiplied by
//! it.
//!
//! The focused window scrolls **pixel-smoothly**: when the server's redraw
//! carries a scroll gesture, [`render`](Renderer::render) takes a [`ScrollFrame`]
//! with a fractional line offset and slides the gesture's band sub-pixel (the
//! client clock drives the interpolation; see `lib`).
//!
//! It paints the full editor surface the [`View`] carries: the tiled windows
//! (gutter + syntax-colored text + per-window status), the split separators,
//! floats (a second on-top pass, with border + title), the tabline, the global
//! status line (`laststatus=3`), the bottom panel (`:messages`/`:ls`), the
//! insert-mode completion popup (with its doc preview), the visual selection,
//! search / incsearch matches, and LSP diagnostic underlines — the GUI analogue
//! of `nxvim-tui`'s `render`, projecting the same `redraw` model.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use glyphon::cosmic_text::{CacheKeyFlags, Fallback, PlatformFallback};
use glyphon::{
    fontdb, Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use nxvim_view::{
    Border, DiagSign, DiagSpan, Geometry, InlayHint, ResizeCursor, StatusSegment, Style, TabData,
    View, VirtChunk, VirtPlacement, WindowRegion, WindowView,
};
use unicode_script::Script;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use winit::window::Window;

use crate::images::{ImageDraw, ImageStatus, ImageStore};
use crate::GuiConfig;

/// Fallback colors when no colorscheme resolved a style — a light grey on a
/// near-black ground, the GUI's equivalent of the TUI's built-in default theme.
const DEFAULT_FG: u32 = 0xc0_c0_c0;
const DEFAULT_BG: u32 = 0x10_10_10;
/// Dim grey for the number gutter when the theme leaves `LineNr` unset.
const DEFAULT_LINE_NR: u32 = 0x60_60_60;
/// Built-in diagnostic underline colors by severity (1=error … 4=hint), used when
/// the colorscheme leaves the `DiagnosticUnderline*` groups unset — the GUI
/// truecolor analogue of the TUI's ANSI severity fallbacks.
const DIAG_ERROR: u32 = 0xe0_6c_75;
const DIAG_WARN: u32 = 0xe5_c0_7b;
const DIAG_INFO: u32 = 0x56_b6_c2;
const DIAG_HINT: u32 = 0x80_80_80;
/// Built-in inlay-hint color when the colorscheme leaves `LspInlayHint` unset — a
/// dim grey (neovim links `LspInlayHint` to a comment-like dimming by default).
/// The GUI truecolor analogue of the TUI's `Color::DarkGray` fallback.
pub const DEFAULT_INLAY: u32 = 0x80_80_80;
/// Line height as a multiple of the font size.
const LINE_SPACING: f32 = 1.30;
/// The multi-cursor accent (a warm amber): the active (primary) cursor's color in
/// MultiCursor placement mode, and the secondary cursors' underline tint in
/// insert/replace mode, so every multi-cursor decoration reads as one family.
/// Mirrors the TUI's `MULTICURSOR_ACCENT`.
const MULTICURSOR_ACCENT: u32 = 0xe5_c0_7b;

/// A cached shaped line: the glyphon buffer plus the frame it was last used in.
struct CacheEntry {
    buffer: Buffer,
    used: u64,
    /// Byte ranges (into the shaped text) of off-grid glyph clusters — emoji and
    /// monochrome symbols whose advance didn't snap to the cell grid. Computed once at
    /// shape time; the text path masks these out of the line and redraws them as
    /// separately-placed, scaled items so they fill their cells without dragging the
    /// rest of the line off-grid (see [`Renderer::push_text`]). Empty for the common
    /// all-narrow line.
    nonsnapped: Vec<(usize, usize)>,
}

/// One styled run within a shaped line: its text, foreground color, and the bold
/// / italic flags that pick a heavier or slanted face when shaping. (Underline,
/// strikethrough, and reverse are *quad* decorations painted separately, not
/// shaping attributes, so they don't live here.) The cache keys on all of these,
/// so a run reshapes only when its text, color, or weight/slant changes.
#[derive(Clone)]
pub struct Seg {
    pub text: String,
    pub fg: u32,
    /// An explicit background, painted as a quad behind the glyph (the shaper only
    /// draws foregrounds). `None` for ordinary text — the window's normal background
    /// already covers it. Set for extmark `virt_text` chunks whose highlight group
    /// carries a `bg` (e.g. a colored inline label), so they read as a filled badge
    /// rather than dark-on-dark. Not part of the shaped-buffer cache key (the shaped
    /// glyphs are bg-independent); see [`Renderer::push_seg_backgrounds`].
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
}

impl Seg {
    /// A plain run with no weight/slant / background — gutter numbers, status text, etc.
    pub fn plain(text: String, fg: u32) -> Self {
        Self {
            text,
            fg,
            bg: None,
            bold: false,
            italic: false,
        }
    }
}

/// One piece of text to draw: a cache key (the shaped buffer), where to put it,
/// and the rect to clip it to (the whole surface for static text; the window's
/// text area for a scroll slide, so partially-scrolled lines clip at the edge).
#[derive(Clone, Copy)]
struct TextItem {
    key: u64,
    x: f32,
    y: f32,
    color: Color,
    bounds: TextBounds,
    /// Render scale for the shaped buffer (1.0 for ordinary text). Used to shrink an
    /// over-wide emoji glyph to fit its cell box (see [`Renderer::push_text`]).
    scale: f32,
}

/// An interpolated scroll-slide frame for the focused window, supplied by the
/// client clock each animation frame. The band is **screen-row based**:
/// `lines`/the overlay arrays are the over-scanned screen rows, and `row_off` is
/// the viewport top's fractional screen-row offset into them (the smoothness comes
/// from *not* rounding it). Band entry `k` is drawn at sub-pixel
/// `y = (k - row_off) * cell_h`, so an interleaved `virt_lines` row simply slides
/// like any other. `cursor_row` is the cursor's fractional band-row offset.
pub struct ScrollFrame<'a> {
    pub row_off: f32,
    pub cursor_row: f32,
    pub lines: &'a [String],
    /// Per-row visual-selection spans for the band (aligned with `lines`), so the
    /// selection slides with the text. `None` rows carry no selection.
    pub selection: &'a [Option<(u16, u16)>],
    /// Per-row secondary multi-cursor selection spans, so they slide too.
    pub secondary_selection: &'a [Vec<(u16, u16)>],
    /// How to clip the selection's moving edge to the interpolated `cursor_row` as
    /// the slide grows: `Some(true)` extending down, `Some(false)` up, `None` for a
    /// pure scroll (cursor unmoved) where the full extent just slides.
    pub sel_clip: Option<bool>,
    pub numbers: &'a [Option<usize>],
    /// Per-row soft-wrap continuation flags for the band, so the gutter blanks the
    /// wrapped rows while the slide animates (sibling of the per-window
    /// `continuation`).
    pub continuation: &'a [bool],
    pub highlights: &'a [Vec<nxvim_view::HlSpan>],
    /// Per-row `hlsearch` match spans for the band (aligned with `lines`), so the
    /// search highlight slides with the text instead of vanishing until the slide
    /// settles. Empty inner slice for rows with no match.
    pub search: &'a [Vec<(u16, u16)>],
    /// Per-row live `incsearch` preview match for the band, or `None`.
    pub incsearch: &'a [Option<(u16, u16)>],
    /// Inline inlay hints for the band (aligned with `lines`), so they slide with
    /// the text instead of vanishing until the slide settles.
    pub inlay_hints: &'a [Vec<InlayHint>],
    /// Extmark `virt_text` placements for the band (aligned with `lines`), so they
    /// slide with the line instead of flashing out and back when the slide settles.
    pub virt_text: &'a [Vec<VirtPlacement>],
    /// Per-row extmark `virt_lines` content (`Some(chunks)` for a virtual row),
    /// interleaved into the band so virtual rows slide with the text.
    pub virt_lines: &'a [Option<Vec<VirtChunk>>],
    /// Per-row diagnostic underline spans / sign-column glyphs for the band, so the
    /// squiggles and signs slide with the text instead of blanking for the slide.
    pub diagnostics: &'a [Vec<DiagSpan>],
    pub diagnostics_signs: &'a [Option<DiagSign>],
    pub styles: &'a [Style],
}

/// Intern a font-family name as a `&'static str`. cosmic-text's [`Fallback`] trait
/// returns `&[&'static str]`, so the user's runtime fallback names must be promoted
/// to `'static`. Interning (rather than leaking per call) bounds the leak to the set
/// of distinct names ever configured, so repeated `:set guifont=…` can't grow it.
fn intern_family(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static POOL: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let pool = POOL.get_or_init(|| Mutex::new(HashSet::new()));
    let mut pool = pool.lock().unwrap();
    if let Some(&existing) = pool.get(name) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    pool.insert(leaked);
    leaked
}

/// Font fallback that tries the user's configured fallback families before the
/// platform's defaults — nxvim's analogue of wezterm's `font_with_fallback`. The
/// user's families (every font after the first in `guifont` / `--font`) are
/// prepended to the platform [`common_fallback`](Fallback::common_fallback), which
/// cosmic-text consults right after the primary family for any glyph whose script
/// has no curated list (`_ => &[]` in the platform tables) — symbols, icons, emoji,
/// box-drawing, most punctuation: exactly the glyphs a coding font tends to miss. A
/// real script (CJK, Arabic, …) still resolves through the platform's curated
/// per-script font first; the user list then covers anything that misses.
struct UserFallback {
    /// The user fallback families (interned) followed by the platform common list.
    common: Vec<&'static str>,
}

impl UserFallback {
    /// Build from the user's fallback families (the `guifont` entries after the
    /// primary), each tried before the platform's own common fallbacks.
    fn new(fallback_families: &[String]) -> Self {
        let common = fallback_families
            .iter()
            .map(|f| intern_family(f))
            .chain(PlatformFallback.common_fallback().iter().copied())
            .collect();
        Self { common }
    }
}

impl Fallback for UserFallback {
    fn common_fallback(&self) -> &[&'static str] {
        &self.common
    }
    // Forbidden and per-script lists are the platform's unchanged — the user list
    // only augments the script-agnostic common path.
    fn forbidden_fallback(&self) -> &[&'static str] {
        PlatformFallback.forbidden_fallback()
    }
    fn script_fallback(&self, script: Script, locale: &str) -> &[&'static str] {
        PlatformFallback.script_fallback(script, locale)
    }
}

/// Build a [`FontSystem`] whose fallback prefers `fallback_families` (the configured
/// families after the primary). Scans the system fonts once via [`FontSystem::new`],
/// then rebuilds onto the same database with the custom [`UserFallback`] (cheap — no
/// second scan).
fn font_system_with_fallback(fallback_families: &[String]) -> FontSystem {
    let (locale, db) = FontSystem::new().into_locale_and_db();
    FontSystem::new_with_locale_and_db_and_fallback(
        locale,
        db,
        UserFallback::new(fallback_families),
    )
}

/// The GPU renderer. Owns the surface and everything needed to paint one frame.
pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: TextAtlas,
    /// The base text layer (tiled windows, chrome). Drawn first.
    text_renderer: TextRenderer,
    /// The overlay text layer (floats and the completion popup). A separate
    /// renderer — hence a separate vertex buffer — so its glyphs draw *after* the
    /// overlay backgrounds, letting those backgrounds occlude the base text they
    /// sit over (glyphon's `prepare` overwrites a renderer's buffer, so one
    /// renderer can't hold two layers within a frame). See [`Renderer::render`].
    overlay_text: TextRenderer,
    viewport: Viewport,
    _cache: Cache,

    rects: RectPipeline,

    /// The image renderer for `'imagepreview'` windows: a textured-quad pipeline
    /// and a path-keyed GPU texture cache. The GUI shares the local filesystem
    /// (like the TUI), so it decodes the `image` marker's file itself.
    image_store: ImageStore,

    /// Shaped-text cache, keyed by line *content* (text + per-span colors), not
    /// screen position — so an unchanged line is shaped once and reused across
    /// frames. Without it, every redraw reshaped every visible row from scratch
    /// (cosmic-text shaping is the per-frame hot path), so a single cursor move on
    /// a maximized window reshaped the whole screen. Now a cursor move that
    /// touches no line text reshapes nothing. Entries unused in a frame are
    /// evicted, so the cache tracks exactly the visible lines.
    cache: HashMap<u64, CacheEntry>,
    /// Frame counter; an entry touched this frame has `used == gen`.
    gen: u64,

    /// The device's `max_texture_dimension_2d`. A surface can't be configured
    /// larger than this in either axis (wgpu raises a non-unwinding validation
    /// panic if it is — what a maximize onto a hi-DPI display would otherwise
    /// trigger), so every requested size is clamped to it.
    max_dim: u32,

    /// Configured font families: the first is the primary (shaped against directly
    /// and measured for the cell); the rest are the fallback chain, baked into
    /// `font_system`'s [`UserFallback`]. Empty means the system monospace.
    fonts: Vec<String>,
    /// Render scale for an emoji / wide fallback glyph (see [`Renderer::push_text`]),
    /// from [`GuiConfig::emoji_scale`].
    emoji_scale: f32,

    /// Device-pixel cell size, measured from the configured font once at startup.
    cell_w: f32,
    cell_h: f32,
    font_size: f32,
    line_height: f32,
    /// The window's scale factor (device pixels per logical pixel), kept so
    /// [`Renderer::set_font`] can rescale a new point size like `new` did.
    scale: f32,

    /// The on-screen text cursor's top-left in physical pixels, captured each frame
    /// as it's painted (the focused-window cursor, or the command-line cursor while
    /// in command mode) — `None` when no cursor is shown (a focused picker/panel owns
    /// it). The client reads it via [`Renderer::ime_cursor_area`] to anchor the IME
    /// candidate window at the caret instead of the window origin.
    cursor_px: Option<(f32, f32)>,

    /// Reusable per-frame draw lists, kept across frames so their heap capacity
    /// survives instead of reallocating the lists from scratch each paint (a
    /// continuously-repainting scroll animation would otherwise re-grow them every
    /// frame). [`render`](Self::render) takes them out via `mem::take` so
    /// `build_frame` can fill them while still borrowing the renderer mutably, then
    /// stores them back; each is cleared (capacity retained) at the next frame's start.
    frame_quads: Vec<Quad>,
    frame_items: Vec<TextItem>,
    frame_overlay_quads: Vec<Quad>,
    frame_overlay_items: Vec<TextItem>,
    frame_image_draws: Vec<ImageDraw>,
}

impl Renderer {
    /// Build the renderer for `window`, rendering with `config`'s font family and
    /// size. Blocks on wgpu's async adapter/device requests via `pollster` (we are
    /// on the synchronous winit setup path).
    pub fn new(
        window: Arc<Window>,
        cfg: &GuiConfig,
        fetch_tx: tokio::sync::mpsc::UnboundedSender<crate::ImageFetch>,
    ) -> anyhow::Result<Self> {
        let size = window.inner_size();
        let scale = window.scale_factor() as f32;

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());
        let surface = instance.create_surface(window)?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::default(),
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))?;
        // Request the adapter's real limits, not `downlevel_defaults()` — the
        // latter caps `max_texture_dimension_2d` at 2048, far below a maximized
        // window on a hi-DPI display, which would abort in `Surface::configure`.
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("nxvim-gui device"),
                required_features: wgpu::Features::empty(),
                required_limits: adapter.limits(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            }))?;
        let max_dim = device.limits().max_texture_dimension_2d;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.clamp(1, max_dim),
            height: size.height.clamp(1, max_dim),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        // A second renderer for the overlay text layer (floats / pmenu), sharing
        // the one atlas; only its own glyph-instance buffer is separate.
        let overlay_text =
            TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);
        let fonts = cfg.fonts.clone();
        let mut font_system = font_system_with_fallback(fonts.get(1..).unwrap_or(&[]));
        let swash_cache = SwashCache::new();

        let font_size = cfg.font_size * scale;
        let line_height = (font_size * LINE_SPACING).round();
        let family = fonts
            .first()
            .map(|s| Family::Name(s))
            .unwrap_or(Family::Monospace);
        let (cell_w, cell_h) = measure_cell(&mut font_system, family, font_size, line_height);

        let rects = RectPipeline::new(&device, format);
        let image_store = ImageStore::new(&device, format, max_dim, fetch_tx);

        Ok(Self {
            surface,
            device,
            queue,
            config,
            font_system,
            swash_cache,
            atlas,
            text_renderer,
            overlay_text,
            viewport,
            _cache: cache,
            rects,
            image_store,
            fonts,
            emoji_scale: cfg.emoji_scale,
            cache: HashMap::new(),
            gen: 0,
            max_dim,
            cell_w,
            cell_h,
            font_size,
            line_height,
            scale,
            cursor_px: None,
            frame_quads: Vec::new(),
            frame_items: Vec::new(),
            frame_overlay_quads: Vec::new(),
            frame_overlay_items: Vec::new(),
            frame_image_draws: Vec::new(),
        })
    }

    /// Re-shape with a new font family list (empty = system monospace) and point
    /// size, re-measuring the cell and dropping the shaped-line cache (its buffers
    /// were shaped at the old metrics). The caller then re-reports the grid (the cell
    /// size, hence `grid_size`, has changed) and repaints. Backs `:set guifont=…`.
    ///
    /// `fonts[0]` is the primary; `fonts[1..]` is the fallback chain. When that chain
    /// changes the [`FontSystem`] is rebuilt with a fresh [`UserFallback`] (reusing
    /// the already-scanned font database — no rescan); a size-only change skips it.
    pub fn set_font(&mut self, fonts: &[String], size_pt: f32) {
        if fonts.get(1..) != self.fonts.get(1..) {
            self.rebuild_font_system(fonts.get(1..).unwrap_or(&[]));
        }
        self.fonts = fonts.to_vec();
        self.font_size = size_pt * self.scale;
        self.line_height = (self.font_size * LINE_SPACING).round();
        let family = self
            .fonts
            .first()
            .map(|s| Family::Name(s))
            .unwrap_or(Family::Monospace);
        let (cell_w, cell_h) = measure_cell(
            &mut self.font_system,
            family,
            self.font_size,
            self.line_height,
        );
        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.cache.clear();
    }

    /// Update the window's scale factor (device pixels per logical pixel) after a
    /// `ScaleFactorChanged` — the window moved onto a different-DPI monitor. The
    /// caller then re-applies the current font ([`Self::set_font`]), which
    /// re-derives the device-pixel metrics from the unchanged point size and
    /// re-measures the cell. A non-finite / non-positive factor is ignored.
    pub fn set_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.scale = scale;
        }
    }

    /// Swap in a [`FontSystem`] whose fallback prefers `fallback_families`, reusing
    /// the current system's font database so no rescan happens. The placeholder is an
    /// empty-database system, immediately dropped once the real database is moved out.
    fn rebuild_font_system(&mut self, fallback_families: &[String]) {
        let placeholder =
            FontSystem::new_with_locale_and_db(String::new(), fontdb::Database::new());
        let old = std::mem::replace(&mut self.font_system, placeholder);
        let (locale, db) = old.into_locale_and_db();
        self.font_system = FontSystem::new_with_locale_and_db_and_fallback(
            locale,
            db,
            UserFallback::new(fallback_families),
        );
    }

    /// The current grid size in cells: `(cols, total_rows)`. The caller reserves
    /// the last row for the command line before reporting the windows-area height
    /// to the server — mirroring the TUI's one reserved chrome row.
    pub fn grid_size(&self) -> (u16, u16) {
        let cols = (self.config.width as f32 / self.cell_w).floor().max(1.0) as u16;
        let rows = (self.config.height as f32 / self.cell_h).floor().max(1.0) as u16;
        (cols, rows)
    }

    /// The absolute screen cell `(col, row)` a physical-pixel pointer position
    /// falls in, using the measured cell size. The event loop turns winit's
    /// `CursorMoved` pixels into the cell the server's mouse hit-test expects.
    pub fn cell_at(&self, x: f64, y: f64) -> (u16, u16) {
        crate::mouse::cell_at(x, y, self.cell_w, self.cell_h)
    }

    /// The resize cursor for the pointer at physical pixel `(px, py)`, or `None`
    /// over an ordinary cell — for showing a resize cursor while hovering a
    /// draggable split separator or dock edge. The chrome row counts mirror
    /// [`Self::build_frame`] so the hit-test agrees with what was painted.
    pub fn resize_cursor_at(&self, view: &View, px: f64, py: f64) -> Option<ResizeCursor> {
        let (col, row) = self.cell_at(px, py);
        let (cols, total_rows) = self.grid_size();
        let geo = Geometry {
            cols,
            rows: total_rows.saturating_sub(1),
            tabline_rows: u16::from(!view.tabline.is_empty()),
            global_status_rows: u16::from(!view.global_status.is_empty()),
        };
        nxvim_view::resize_handle_at(view, geo, row, col)
    }

    /// The measured cell size in physical pixels, for turning a trackpad's
    /// pixel-precise scroll delta into whole-line wheel notches.
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// The text cursor's cell rect in physical pixels `(x, y, w, h)` from the last
    /// painted frame, or `None` when no cursor was shown. The client feeds this to
    /// `Window::set_ime_cursor_area` so the IME candidate window opens at the caret.
    pub fn ime_cursor_area(&self) -> Option<(f32, f32, f32, f32)> {
        self.cursor_px
            .map(|(x, y)| (x, y, self.cell_w, self.cell_h))
    }

    /// Reconfigure the surface after a window resize. The size is clamped to the
    /// device's max texture dimension so a maximize/zoom onto a large hi-DPI
    /// display can't exceed it and abort in `Surface::configure`.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let (w, h) = (width.min(self.max_dim), height.min(self.max_dim));
        if (w, h) == (self.config.width, self.config.height) {
            return; // unchanged — skip the swapchain reconfigure (winit can
                    // re-report the same size on focus/DPI churn)
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    /// Build the glyphon [`TextArea`]s for `items`, each borrowing its shaped
    /// buffer from `cache` (an item whose buffer was evicted is skipped). An
    /// associated fn, not a closure, so a caller can build a layer's areas while
    /// still free to mutably borrow the renderer's other fields for `prepare`.
    fn text_areas<'a>(
        cache: &'a HashMap<u64, CacheEntry>,
        items: &'a [TextItem],
    ) -> Vec<TextArea<'a>> {
        // Pre-size to the item count (an exact upper bound — only evicted entries are
        // dropped) so the per-frame area list doesn't grow by repeated reallocation.
        let mut areas = Vec::with_capacity(items.len());
        areas.extend(items.iter().filter_map(|it| {
            cache.get(&it.key).map(|e| TextArea {
                buffer: &e.buffer,
                left: it.x,
                top: it.y,
                scale: it.scale,
                bounds: it.bounds,
                default_color: it.color,
                custom_glyphs: &[],
            })
        }));
        areas
    }

    /// Hand a remote preview's fetched bytes (an `nxvim_image_read` reply, routed from
    /// the IO thread) to the image store, so the next paint decodes them. The caller
    /// requests a repaint afterward.
    pub fn deliver_image(
        &mut self,
        path: String,
        version: (u64, u64),
        result: Result<Vec<u8>, String>,
    ) {
        self.image_store.deliver(path, version, result);
    }

    /// Drop all cached image state (GPU textures + fetched remote bytes) — used on a
    /// `:connect` swap, where the new session's paths are unrelated to the old's.
    pub fn clear_images(&mut self) {
        self.image_store.clear();
    }

    /// Paint one frame from `view`. When `scroll` is set, the focused window's
    /// text slides at the interpolated offset (smooth scrolling). Returns `Err`
    /// only on an unrecoverable surface error; a transient `Lost`/`Outdated`
    /// reconfigures and skips.
    pub fn render(
        &mut self,
        view: &View,
        scroll: Option<&ScrollFrame>,
        doc_scroll: u16,
    ) -> anyhow::Result<()> {
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Recomputed below as the cursor is painted (or left `None` when a
        // picker/panel owns it), so the IME area always reflects this frame.
        self.cursor_px = None;

        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );

        // Project the view into two layers. The base layer is the tiled windows
        // and chrome; the overlay layer is the floats and the completion popup,
        // which sit *over* base text. Each layer is solid fills (`quads`) plus text
        // (`items`) referencing shaped buffers in `self.cache`. Drawing base
        // quads → base text → overlay quads → overlay text lets an overlay's opaque
        // background occlude the base text beneath it (a single text pass after all
        // quads can't — glyphs always paint over quads, so the float would look
        // transparent). Bump the frame counter first so the build marks the lines
        // it touches and we can evict the rest.
        self.gen = self.gen.wrapping_add(1);
        let bg = style_bg(&view.normal).unwrap_or(DEFAULT_BG);
        // Reuse last frame's draw lists (their capacity is retained); taking them out
        // of `self` lets `build_frame` fill them while still borrowing the renderer
        // mutably. They're stored back below so the next frame reuses the allocation.
        let mut quads = std::mem::take(&mut self.frame_quads);
        let mut items = std::mem::take(&mut self.frame_items);
        let mut overlay_quads = std::mem::take(&mut self.frame_overlay_quads);
        let mut overlay_items = std::mem::take(&mut self.frame_overlay_items);
        let mut image_draws = std::mem::take(&mut self.frame_image_draws);
        quads.clear();
        items.clear();
        overlay_quads.clear();
        overlay_items.clear();
        image_draws.clear();
        // Decode/upload every preview image *before* building the frame, so
        // `build_window` knows decode failures the same frame and paints the
        // `[image: …]` placeholder for them (a one-frame lag could otherwise never
        // repaint — redraws are event-driven). Disjoint field borrows.
        let live: Vec<&nxvim_view::ImageData> = view
            .windows
            .iter()
            .filter_map(|w| w.image.as_ref())
            .collect();
        self.image_store.ensure(&self.device, &self.queue, &live);
        self.build_frame(
            view,
            scroll,
            doc_scroll,
            &mut quads,
            &mut items,
            &mut overlay_quads,
            &mut overlay_items,
            &mut image_draws,
        );
        // Build this frame's textured quads from the decoded cache.
        let (sw, sh) = (self.config.width as f32, self.config.height as f32);
        self.image_store
            .build_quads(&self.device, &self.queue, &image_draws, sw, sh);
        // Drop buffers for lines that scrolled/changed off-screen this frame (both
        // layers were built above, so both have marked the entries they use).
        let gen = self.gen;
        self.cache.retain(|_, e| e.used == gen);

        // Prepare the base text layer, then the overlay text layer, each into its
        // own renderer (separate glyph buffers; one shared atlas).
        let base_areas = Self::text_areas(&self.cache, &items);
        self.text_renderer
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                base_areas,
                &mut self.swash_cache,
            )
            .map_err(|e| anyhow::anyhow!("glyphon prepare: {e:?}"))?;
        let overlay_areas = Self::text_areas(&self.cache, &overlay_items);
        self.overlay_text
            .prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                overlay_areas,
                &mut self.swash_cache,
            )
            .map_err(|e| anyhow::anyhow!("glyphon prepare (overlay): {e:?}"))?;

        self.rects.upload(
            &self.device,
            &self.queue,
            &quads,
            &overlay_quads,
            self.config.width as f32,
            self.config.height as f32,
        );

        // The draw lists are fully consumed (uploaded / prepared); hand them back so
        // next frame reuses their capacity. Done before the render pass so a transient
        // pass error can't strand the allocation.
        self.frame_quads = quads;
        self.frame_items = items;
        self.frame_overlay_quads = overlay_quads;
        self.frame_overlay_items = overlay_items;
        self.frame_image_draws = image_draws;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("nxvim-gui"),
            });
        {
            let clear = srgb_u32_to_linear(bg);
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("nxvim-gui pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0] as f64,
                            g: clear[1] as f64,
                            b: clear[2] as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            // Base layer first, then the overlay layer on top: each layer's quads
            // then its text, so an overlay background occludes the base text under it.
            self.rects.draw_base(&mut pass);
            // Preview images sit on the base layer, over the window background and
            // under floats/popups (the overlay layer, drawn below).
            self.image_store.draw(&mut pass);
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)
                .map_err(|e| anyhow::anyhow!("glyphon render: {e:?}"))?;
            self.rects.draw_overlay(&mut pass);
            self.overlay_text
                .render(&self.atlas, &self.viewport, &mut pass)
                .map_err(|e| anyhow::anyhow!("glyphon render (overlay): {e:?}"))?;
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        frame.present();
        self.atlas.trim();
        Ok(())
    }

    /// Walk the view and append its quads and shaped text rows to the draw lists.
    ///
    /// Layout mirrors the server's `relayout` (and the TUI's `render`): the client
    /// reports only the command row as reserved, and the server shrinks the
    /// windows area itself for the tabline (top), the global status line and the
    /// panel (bottom), emitting window rects **relative to the windows-area
    /// origin**. So the GUI offsets every window/separator/cursor by that origin
    /// (`(0, tabline_rows)`) and paints the chrome regions around it.
    #[allow(clippy::too_many_arguments)]
    fn build_frame(
        &mut self,
        view: &View,
        scroll: Option<&ScrollFrame>,
        doc_scroll: u16,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
        overlay_quads: &mut Vec<Quad>,
        overlay_items: &mut Vec<TextItem>,
        image_draws: &mut Vec<ImageDraw>,
    ) {
        let (cols, total_rows) = self.grid_size();
        let cmd_row = total_rows.saturating_sub(1);

        // Chrome rows the windows area gives up: the tabline at the top (≥2 tabs)
        // and the global status line at the bottom. The server already sized the
        // window rects to fit what's left.
        let tabline_rows = u16::from(!view.tabline.is_empty());
        let global_status_rows = u16::from(!view.global_status.is_empty());

        // The permanent dock bands. Each open dock reserves its content extent plus
        // one separator cell toward the main area; the frame stacks as `[top dock]
        // [tabline][left|main|right][global status][bottom dock][cmd]`. With no dock
        // open every band is 0 and this collapses to the pre-dock layout.
        let res = |n: u16| if n > 0 { n + 1 } else { 0 };
        let (dl, dr, dt, db) = (
            view.dock_left,
            view.dock_right,
            view.dock_top,
            view.dock_bottom,
        );
        let (top_band, bottom_band, left_band, right_band) = (res(dt), res(db), res(dl), res(dr));
        let mid_y = top_band + tabline_rows;
        let mid_h = cmd_row
            .saturating_sub(top_band)
            .saturating_sub(tabline_rows)
            .saturating_sub(global_status_rows)
            .saturating_sub(bottom_band)
            .max(1);
        let main_x = left_band;
        let main_w = cols
            .saturating_sub(left_band)
            .saturating_sub(right_band)
            .max(1);
        let bottom_y = mid_y + mid_h + global_status_rows;
        // Each open dock reserves the first row of its band for its own tabline,
        // shifting that dock's window content down a row (mirroring the row the core
        // relayout removed from the dock tree). Left/right docks gate on the column
        // height (`mid_h`), top/bottom on their band height.
        let rt = &view.region_tablines;
        let tl_top = u16::from(!rt.top.tabs.is_empty() && dt > 1);
        let tl_bottom = u16::from(!rt.bottom.tabs.is_empty() && db > 1);
        let tl_left = u16::from(!rt.left.tabs.is_empty() && mid_h > 1);
        let tl_right = u16::from(!rt.right.tabs.is_empty() && mid_h > 1);
        let dock_left_y = mid_y;
        let dock_right_y = mid_y;
        let dock_top_y = 0;
        let dock_bottom_y = bottom_y + (bottom_band - db);
        // Each region's content-cell origin (the dock separator faces the main area,
        // so the bottom/right docks' content starts one cell past the band edge),
        // shifted past the dock's own tabline row where present.
        let region_origin = |region: WindowRegion| -> (u16, u16) {
            match region {
                WindowRegion::Main => (main_x, mid_y),
                WindowRegion::DockLeft => (0, dock_left_y + tl_left),
                WindowRegion::DockRight => (cols.saturating_sub(dr), dock_right_y + tl_right),
                WindowRegion::DockTop => (0, dock_top_y + tl_top),
                WindowRegion::DockBottom => (0, dock_bottom_y + tl_bottom),
            }
        };

        // The global (main) tabline on the row below the top dock.
        if tabline_rows > 0 {
            self.build_tabline(view, cols, quads, items);
        }
        // Each dock's own tabline at its band's first row.
        self.build_dock_tablines(
            view,
            &[
                (0, dock_top_y, cols, &rt.top, tl_top > 0),
                (0, dock_bottom_y, cols, &rt.bottom, tl_bottom > 0),
                (0, dock_left_y, dl, &rt.left, tl_left > 0),
                (
                    cols.saturating_sub(dr),
                    dock_right_y,
                    dr,
                    &rt.right,
                    tl_right > 0,
                ),
            ],
            quads,
            items,
        );

        // Tiled windows first; floats overlay them in a second pass so they sit on
        // top. Only the focused window slides; the rest paint from the live view.
        for win in view.windows.iter().filter(|w| !w.floating) {
            let (ox, oy) = region_origin(win.region);
            let rect = match win.rect {
                Some(r) => (ox + r.x, oy + r.y, r.width, r.height),
                None => (main_x, mid_y, main_w, mid_h),
            };
            // `'padding'` insets the content box by a per-side margin; the server
            // already sized this window's rows/cursor to the inset area.
            let rect = pad_rect(rect, win.padding);
            let win_scroll = if win.focused { scroll } else { None };
            self.build_window(view, win, win_scroll, rect, quads, items, image_draws);
        }

        // Separators between splits — thin lines, each offset by its region's
        // content origin like the windows they divide. Coloured by the theme's
        // `WinSeparator` (its foreground is the line colour — e.g. catppuccin's dim
        // `crust`), falling back to the status-line background when the colorscheme
        // leaves `WinSeparator` undefined.
        let sep = srgb_to_color(
            style_fg(&view.win_separator)
                .or_else(|| style_bg(&view.win_separator))
                .or_else(|| style_bg(&view.status_line))
                .unwrap_or(0x40_40_40),
        );
        let mut line = |px: f32, py: f32, w: f32, h: f32| {
            quads.push(Quad {
                x: px,
                y: py,
                w,
                h,
                color: color_to_rgba(sep),
            });
        };
        for s in &view.separators {
            let (ox, oy) = region_origin(s.region);
            let (cx, cy) = self.cell_px(ox + s.x, oy + s.y);
            if s.vertical {
                line(cx, cy, self.cell_w * 0.12, self.cell_h * s.length as f32);
            } else {
                line(cx, cy, self.cell_w * s.length as f32, self.cell_h * 0.12);
            }
        }
        // The border line between each open dock and the main area. Drawn heavier
        // than the split separators above (the GUI analogue of the TUI/web's heavy
        // `━`/`┃` glyphs) so a permanent dock edge reads as distinct from an ordinary
        // window split.
        const DOCK_EDGE: f32 = 0.2;
        if dt > 0 {
            let (cx, cy) = self.cell_px(0, dt);
            line(cx, cy, self.cell_w * cols as f32, self.cell_h * DOCK_EDGE);
        }
        if db > 0 {
            let (cx, cy) = self.cell_px(0, bottom_y);
            line(cx, cy, self.cell_w * cols as f32, self.cell_h * DOCK_EDGE);
        }
        if dl > 0 {
            let (cx, cy) = self.cell_px(dl, mid_y);
            line(cx, cy, self.cell_w * DOCK_EDGE, self.cell_h * mid_h as f32);
        }
        if dr > 0 {
            let (cx, cy) = self.cell_px(cols.saturating_sub(dr + 1), mid_y);
            line(cx, cy, self.cell_w * DOCK_EDGE, self.cell_h * mid_h as f32);
        }

        // Floats on top, in list order (the server already sorts them by zindex) —
        // into the overlay layer so their opaque background occludes the tiled-window
        // text beneath them (drawn after base text; see `render`).
        for win in view.windows.iter().filter(|w| w.floating) {
            self.build_float(
                view,
                win,
                region_origin(win.region),
                overlay_quads,
                overlay_items,
                image_draws,
            );
        }

        // The global status line (`laststatus=3`), docked just below the main band.
        if global_status_rows > 0 {
            let row = mid_y + mid_h;
            self.build_status_row(&view.global_status, (0, row), cols, view, quads, items);
        }

        // The insert-mode completion popup, anchored over the focused window (in its
        // region) — in the overlay layer (like floats) so it sits opaque over the
        // window text.
        let focus_origin = view
            .focused()
            .map(|w| region_origin(w.region))
            .unwrap_or((main_x, mid_y));
        self.build_pmenu(view, focus_origin, doc_scroll, overlay_quads, overlay_items);

        // The floating selectable-list menu (`nx.ui.select`), in the same overlay
        // layer and anchored the same way (the focused window's region origin) — except
        // the command-line wildmenu, which anchors to the command-line row (`cmd_row`).
        self.build_menu(view, focus_origin, cmd_row, overlay_quads, overlay_items);

        // The list-less content float (`nx.ui.float`; LSP hover / signature help),
        // same overlay layer, anchored at the focused window's region origin.
        self.build_content_float(view, focus_origin, overlay_quads, overlay_items);

        // The global command / message line on the reserved bottom row.
        self.build_cmdline(view, cmd_row, quads, items);
    }

    /// Paint one window: gutter numbers, text rows (syntax-colored), the visual
    /// selection and search highlights, its status bar, and — if focused — the
    /// cursor.
    #[allow(clippy::too_many_arguments)]
    fn build_window(
        &mut self,
        view: &View,
        win: &WindowView,
        scroll: Option<&ScrollFrame>,
        rect: (u16, u16, u16, u16),
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
        image_draws: &mut Vec<ImageDraw>,
    ) {
        // `rect` is the window's content rect in absolute screen cells, already
        // offset by the windows-area origin (and inset past a float's border).
        let (ox, oy, wcols, wrows) = rect;

        // An image-preview buffer (`'imagepreview'`): blit the picture over the
        // whole text body and skip the gutter / text machinery entirely (the buffer
        // is empty — there's no meaningful cursor). The store decodes/uploads the
        // file from the marker's path; a decode failure paints a text placeholder.
        if let Some(image) = &win.image {
            let text_rows = wrows.saturating_sub(u16::from(win.status_visible));
            let (px, py) = self.cell_px(ox, oy);
            let area = (
                px,
                py,
                wcols as f32 * self.cell_w,
                text_rows as f32 * self.cell_h,
            );
            match self.image_store.status(image) {
                ImageStatus::Ready => image_draws.push(ImageDraw {
                    area,
                    image: image.clone(),
                }),
                // No texture to blit: a remote fetch still in flight reads as
                // "loading"; anything else (a decode failure, an errored fetch) is a
                // hard "cannot read". Either way paint a one-line text placeholder.
                status => {
                    let msg = match status {
                        ImageStatus::Loading => {
                            format!("[image: loading {}]", image.path)
                        }
                        _ => format!("[image: cannot read {}]", image.path),
                    };
                    let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
                    self.push_plain(items, &msg, (px, py), fg, self.full_bounds());
                }
            }
            if win.status_visible && (oy + wrows) as usize > 0 {
                let srow = oy + wrows.saturating_sub(1);
                self.build_status_row(&win.status, (ox, srow), wcols, view, quads, items);
            }
            return;
        }
        // Left columns: a 2-cell diagnostic sign column (vim's `signcolumn`, when
        // this buffer has diagnostics and signs are on), then the number gutter,
        // then the text — so the text origin shifts past both. Cursor, pmenu, and
        // mouse hit-test all derive from the same `text_x0` (see `pmenu_hit`).
        let sign_w = win.sign_width;
        let gutter = if win.number || win.relativenumber {
            win.number_width
        } else {
            0
        };
        let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
        // The fold-marker gutter (vim's foldcolumn) sits at the very left, before
        // the sign and number columns, so every other origin shifts right by it.
        let fold_w = win.foldcolumn_width;
        let sign_x0 = ox + fold_w;
        let gutter_x0 = ox + fold_w + sign_w;
        let text_x0 = ox + fold_w + sign_w + gutter;
        // Text-area height in cells (the window minus its status row).
        let text_rows = wrows.saturating_sub(u16::from(win.status_visible));

        match scroll {
            // Sliding: paint the gesture's band at the fractional offset, clipped
            // to the text area so a partially-scrolled line cuts off at the edge.
            // The band carries no selection/search overlay — those reappear when
            // the slide settles. Band lines are stable content, so the shaped
            // buffers are cache hits frame to frame.
            Some(s) => {
                let clip = self.text_bounds(ox, oy, wcols, text_rows);
                let slide_bg = style_bg(&view.normal).unwrap_or(DEFAULT_BG);
                let sel_bg = style_bg(&view.visual).unwrap_or(0x33_47_5b);
                let search_bg = style_bg(&view.search_style).unwrap_or(0x6a_5a_1a);
                let inc_bg = style_bg(&view.incsearch_style).unwrap_or(0x8a_6d_1a);
                // The cursor's band row tracks the interpolated slide, so relative
                // numbers stay in step with the moving text and the selection's
                // moving edge clips to it (see `sel_clip`). `numbers` at that band
                // row gives the cursor's 1-based buffer line.
                let cur_row0 = s.cursor_row.round() as usize; // band-row of the cursor
                let current_line = s.numbers.get(cur_row0).copied().flatten().unwrap_or(1);
                for (k, raw) in s.lines.iter().enumerate() {
                    // Screen-row band: row `k` sits `k - row_off` rows below the
                    // viewport top, drawn sub-pixel. Interleaved `virt_lines` rows are
                    // just more band rows, so they slide with everything else.
                    let row = k as f32 - s.row_off;
                    if row <= -1.0 || row >= text_rows as f32 {
                        continue; // fully outside the text area
                    }
                    let y = (oy as f32 + row) * self.cell_h;
                    // A `virt_lines` virtual row rides the band now: paint its chunk
                    // text at the sub-pixel offset and skip the rest of the row (no
                    // gutter/selection/search). Chunk backgrounds settle in when the
                    // slide ends, like other mid-slide virtual text.
                    if let Some(Some(chunks)) = s.virt_lines.get(k) {
                        let segs: Vec<Seg> = chunks
                            .iter()
                            .map(|(t, id)| virt_chunk_seg(t, *id, s.styles, fg))
                            .collect();
                        let x = text_run_origin(text_x0, win.leftcol) as f32 * self.cell_w;
                        self.push_text(items, &segs, (x, y), fg, clip);
                        continue;
                    }
                    let inlay = s.inlay_hints.get(k).map(Vec::as_slice).unwrap_or(&[]);
                    // Visual selection rides the slide. Its moving edge grows with the
                    // scroll: rows the interpolated cursor hasn't reached yet are not
                    // highlighted (the band carries the destination extent), so the
                    // selection extends together with the slide instead of flashing to
                    // full extent on frame 0. A pure scroll (`sel_clip == None`) slides
                    // the whole extent. The clip is in band-row space (`k` vs the
                    // cursor's band row). The quad clamps to the text area vertically
                    // (quads aren't scissored) so a partial row cuts off at the edge.
                    if let Some(Some(span)) = s.selection.get(k) {
                        let hidden = match s.sel_clip {
                            Some(true) => k > cur_row0,
                            Some(false) => k < cur_row0,
                            None => false,
                        };
                        if !hidden {
                            self.push_span_quad_at(
                                quads,
                                text_x0,
                                y,
                                *span,
                                win.leftcol,
                                inlay,
                                sel_bg,
                                clip.top as f32,
                                clip.bottom as f32,
                            );
                        }
                    }
                    // Secondary multi-cursor selections ride the slide too, painted
                    // with the same `Visual` background as the primary.
                    if let Some(spans) = s.secondary_selection.get(k) {
                        for span in spans {
                            self.push_span_quad_at(
                                quads,
                                text_x0,
                                y,
                                *span,
                                win.leftcol,
                                inlay,
                                sel_bg,
                                clip.top as f32,
                                clip.bottom as f32,
                            );
                        }
                    }
                    // Search matches ride the slide too, so `hlsearch`/`incsearch`
                    // keep highlighting the moving text instead of blinking off
                    // until the slide settles. The live incsearch preview paints on
                    // top of the persistent matches, as on the settled path.
                    if let Some(spans) = s.search.get(k) {
                        for span in spans {
                            self.push_span_quad_at(
                                quads,
                                text_x0,
                                y,
                                *span,
                                win.leftcol,
                                inlay,
                                search_bg,
                                clip.top as f32,
                                clip.bottom as f32,
                            );
                        }
                    }
                    if let Some(Some(span)) = s.incsearch.get(k) {
                        self.push_span_quad_at(
                            quads,
                            text_x0,
                            y,
                            *span,
                            win.leftcol,
                            inlay,
                            inc_bg,
                            clip.top as f32,
                            clip.bottom as f32,
                        );
                    }
                    let display = expand_tabs(raw, win.tabstop.max(1) as usize);
                    // Blank the number on a soft-wrap continuation row (vim shows it
                    // on the line's first row only); `numbers` still carries the line.
                    if gutter > 0 && !s.continuation.get(k).copied().unwrap_or(false) {
                        if let Some(Some(n)) = s.numbers.get(k) {
                            let pos = (gutter_x0 as f32 * self.cell_w, y);
                            self.push_gutter(items, (*n, current_line), win, view, pos, clip);
                        }
                    }
                    let hl = s.highlights.get(k).map(Vec::as_slice).unwrap_or(&[]);
                    let vtext = s.virt_text.get(k).map(Vec::as_slice).unwrap_or(&[]);
                    let mut segments =
                        row_segments(&display, hl, s.styles, fg, slide_bg, win.leftcol);
                    // Splice the band row's inlay hints and inline/overlay virt_text in,
                    // like the settled path, so they slide with the text instead of
                    // flashing out and back when the slide settles. (eol / right_align /
                    // chunk backgrounds aren't painted mid-slide — a brief transient on
                    // the ~150ms animation; they settle into place when it ends.)
                    segments = if vtext.is_empty() {
                        splice_inlay(segments, inlay, win.leftcol, s.styles)
                    } else {
                        apply_row_virt(segments, inlay, vtext, win.leftcol, s.styles, fg)
                    };
                    let x = text_run_origin(text_x0, win.leftcol) as f32 * self.cell_w;
                    self.push_text(items, &segments, (x, y), fg, clip);

                    // Diagnostic sign + underlines ride the band too, at the row's
                    // sub-pixel offset, so they slide with the text instead of
                    // blanking out for the slide (the settled path paints the same).
                    if sign_w > 0 {
                        if let Some(Some((glyph, severity, id))) = s.diagnostics_signs.get(k) {
                            let color = diag_color(s.styles, *id, *severity, false);
                            let text = pad_to_width(glyph, sign_w as usize);
                            self.push_plain(
                                items,
                                &text,
                                (sign_x0 as f32 * self.cell_w, y),
                                color,
                                clip,
                            );
                        }
                    }
                    if let Some(diags) = s.diagnostics.get(k) {
                        for (ds, de, severity, id) in diags {
                            let color = diag_color(s.styles, *id, *severity, true);
                            self.push_underline_at(
                                quads,
                                text_x0,
                                y,
                                (*ds, *de),
                                win.leftcol,
                                inlay,
                                color,
                            );
                        }
                    }
                }
            }
            // Settled: paint the live viewport with selection/search overlays.
            None => {
                let full = self.full_bounds();
                // The window's text area, so a horizontally-scrolled (or just long)
                // line clips at this window's edge instead of spilling over the
                // gutter on the left or the next split on the right.
                let text_clip =
                    self.text_bounds(text_x0, oy, (ox + wcols).saturating_sub(text_x0), text_rows);
                // The whole window cell rect (gutter + sign + text). The gutter and
                // sign glyphs sit left of `text_x0`, so they clip here rather than to
                // the surface — a window squeezed narrower than its gutter (a dock
                // dragged to its minimum) then truncates instead of bleeding its line
                // numbers over the separator and the neighbouring region.
                let win_clip = self.text_bounds(ox, oy, wcols, text_rows);
                let sel_bg = style_bg(&view.visual).unwrap_or(0x33_47_5b);
                let search_bg = style_bg(&view.search_style).unwrap_or(0x6a_5a_1a);
                // This window's `'winhighlight'` override of `Normal` (else global),
                // so a dock with `winhighlight = 'Normal:NormalSB'` paints its own bg.
                let normal_bg = style_bg(&win.normal(view)).unwrap_or(DEFAULT_BG);
                // `'cursorline'` tint: the colorscheme's `CursorLine` background (with
                // any per-window `winhighlight` override), or a subtle lightening of the
                // window background when it leaves it undefined, so the cursor row still
                // reads as highlighted out of the box.
                let cursorline_bg =
                    style_bg(&win.cursor_line_bg(view)).unwrap_or_else(|| lighten(normal_bg, 0x12));
                // A window whose `'winhighlight'` overrides `Normal` (a dock styled
                // `Normal:NormalSB`) repaints its whole cell rect with its own
                // background, so its empty cells / gutter read with the override and
                // not the global surface fill. Gated on the override so an ordinary
                // window adds no quad and renders exactly as before.
                if win.chrome.normal.is_some() {
                    self.fill_rect(quads, ox, oy, wcols, text_rows, normal_bg);
                }
                for (i, raw) in win.lines.iter().enumerate() {
                    let row = oy as usize + i;

                    // `'cursorline'`: tint the cursor's screen row across the whole
                    // window (sign + gutter + text). Pushed before the selection /
                    // search quads so those win on the cells they cover; every quad
                    // sits under the glyphs, so the gutter number and text paint on top.
                    if win.cursorline && i == win.cursor_row as usize {
                        self.fill_cells(quads, ox, row as u16, wcols, cursorline_bg);
                    }

                    // A `virt_lines` virtual row: a whole extra screen line of extmark
                    // chunk text, interleaved by core (it has `numbers[i] == None`, so no
                    // gutter number, and an empty `lines[i]`). Paint the chunks from the
                    // text origin (no selection / search / cursor / diagnostics) and skip
                    // the rest of the row. `virt_lines_leftcol` (start over the gutter) is
                    // a later refinement — today virtual rows begin at the text body.
                    if let Some(Some(chunks)) = win.virt_lines.get(i) {
                        let segs: Vec<Seg> = chunks
                            .iter()
                            .map(|(t, id)| virt_chunk_seg(t, *id, &view.styles, fg))
                            .collect();
                        let pos = self.cell_px(text_x0, row as u16);
                        self.push_seg_backgrounds(
                            quads,
                            &segs,
                            text_x0,
                            row,
                            (ox + wcols) as f32 * self.cell_w,
                        );
                        self.push_text(items, &segs, pos, fg, text_clip);
                        continue;
                    }

                    // Tab-expand so a char index equals a screen column (ASCII
                    // path); the highlight spans on the wire are screen-column based.
                    let display = expand_tabs(raw, win.tabstop.max(1) as usize);

                    // Inline LSP inlay hints on this row, spliced into the text below
                    // (pushing later glyphs right). The column-keyed overlays here —
                    // selection / search / diagnostics / cursor — must shift by the
                    // same inserted width, so they ride through `inlay` (an empty
                    // slice, the common case, reduces every shift to zero).
                    let inlay = win.inlay_hints.get(i).map(Vec::as_slice).unwrap_or(&[]);

                    // Selection band(s) for this row.
                    if let Some(Some(span)) = win.selection.get(i) {
                        self.push_span_quad(quads, text_x0, row, *span, win.leftcol, inlay, sel_bg);
                    }
                    // Secondary multi-cursor selections, painted with the same
                    // `Visual` background as the primary (the server resolves which
                    // cursor owns which span; the client paints them alike).
                    if let Some(spans) = win.secondary_selection.get(i) {
                        for span in spans {
                            self.push_span_quad(
                                quads,
                                text_x0,
                                row,
                                *span,
                                win.leftcol,
                                inlay,
                                sel_bg,
                            );
                        }
                    }
                    // Search matches for this row.
                    if let Some(spans) = win.search.get(i) {
                        for span in spans {
                            self.push_span_quad(
                                quads,
                                text_x0,
                                row,
                                *span,
                                win.leftcol,
                                inlay,
                                search_bg,
                            );
                        }
                    }
                    // The live incsearch preview match rides on top of `hlsearch`.
                    if let Some(Some(span)) = win.incsearch.get(i) {
                        let inc_bg = style_bg(&view.incsearch_style).unwrap_or(0x8a_6d_1a);
                        self.push_span_quad(quads, text_x0, row, *span, win.leftcol, inlay, inc_bg);
                    }

                    // The diagnostic sign in the far-left 2-cell column (when this
                    // window reserved one), painted before the gutter so the most
                    // severe glyph for the line sits at the window's left edge.
                    if sign_w > 0 {
                        if let Some(Some((glyph, severity, id))) = win.diagnostics_signs.get(i) {
                            let color = diag_color(&view.styles, *id, *severity, false);
                            let text = pad_to_width(glyph, sign_w as usize);
                            self.push_plain(
                                items,
                                &text,
                                self.cell_px(sign_x0, row as u16),
                                color,
                                win_clip,
                            );
                        }
                    }

                    // The fold-marker gutter at the far left (`-`/`│`/`+`), in the
                    // line-number color.
                    if fold_w > 0 {
                        if let Some(marker) = win.foldcolumn.get(i).filter(|s| !s.trim().is_empty())
                        {
                            let color = style_fg(&view.line_nr).unwrap_or(fg);
                            self.push_plain(
                                items,
                                marker,
                                self.cell_px(ox, row as u16),
                                color,
                                win_clip,
                            );
                        }
                    }

                    // Gutter number for this row, honoring number/relativenumber
                    // and the cursor-line highlight.
                    // Blank the number on a soft-wrap continuation row (vim shows it
                    // on the line's first row only); `numbers` still carries the line.
                    if gutter > 0 && !win.continuation.get(i).copied().unwrap_or(false) {
                        if let Some(Some(n)) = win.numbers.get(i) {
                            let pos = self.cell_px(gutter_x0, row as u16);
                            self.push_gutter(
                                items,
                                (*n, win.cursor_line),
                                win,
                                view,
                                pos,
                                win_clip,
                            );
                        }
                    }

                    // The text itself, syntax-colored from the row's highlights, with
                    // the inlay hints spliced in at their anchor columns.
                    let hl = win.highlights.get(i).map(Vec::as_slice).unwrap_or(&[]);
                    // Reverse fills (a foreground-colored quad behind the inverted
                    // glyph) go under the text; underline/strikethrough rules go over
                    // it. Both walk the same highlight spans (see the methods).
                    self.push_reverse_fills(quads, win, view, text_x0, row, hl, inlay);
                    let vtext = win.virt_text.get(i).map(Vec::as_slice).unwrap_or(&[]);
                    // `~` end-of-buffer filler rows (`numbers[i] == None`, no tokens)
                    // paint with the theme's `EndOfBuffer` foreground rather than the
                    // `Normal` text fg — matching the TUI and vim's default look.
                    let row_fg = if matches!(win.numbers.get(i), Some(None)) {
                        style_fg(&view.end_of_buffer).unwrap_or(fg)
                    } else {
                        fg
                    };
                    let mut segments =
                        row_segments(&display, hl, &view.styles, row_fg, normal_bg, win.leftcol);
                    // Inline + overlay extmark `virt_text` transform the base segments
                    // (shift / overwrite); inlay hints splice in too. The common no-virt
                    // row keeps the cheaper inlay-only splice (tested path, untouched).
                    segments = if vtext.is_empty() {
                        splice_inlay(segments, inlay, win.leftcol, &view.styles)
                    } else {
                        apply_row_virt(segments, inlay, vtext, win.leftcol, &view.styles, fg)
                    };
                    // The run begins at the first *visible* column (`row_segments`
                    // already dropped the off-screen-left ones), so it starts at the
                    // text origin — not `leftcol` cells back over the gutter. Clip it
                    // to this window's text area so a line wider than the window cuts
                    // off at the edge instead of bleeding into the next split.
                    let pos = self.cell_px(text_run_origin(text_x0, win.leftcol), row as u16);
                    self.push_text(items, &segments, pos, row_fg, text_clip);
                    // Background quads for any segment whose group set a `bg` — a
                    // buffer-text span (a diff line tint, a colorscheme group with a
                    // background) or a virt_text chunk (inline / overlay badge). The
                    // shaper only draws fgs, and all quads render under all glyphs, so
                    // this paints the fill behind the run.
                    self.push_seg_backgrounds(
                        quads,
                        &segments,
                        text_run_origin(text_x0, win.leftcol),
                        row,
                        (ox + wcols) as f32 * self.cell_w,
                    );
                    self.push_attr_rules(quads, win, view, text_x0, row, hl, inlay);

                    // LSP diagnostic underlines, painted last so they survive over
                    // the syntax/selection: a thin colored rule under the cells.
                    if let Some(diags) = win.diagnostics.get(i) {
                        for (s, e, severity, id) in diags {
                            let color = diag_color(&view.styles, *id, *severity, true);
                            self.push_underline(
                                quads,
                                text_x0,
                                row,
                                (*s, *e),
                                win.leftcol,
                                inlay,
                                color,
                            );
                        }
                    }

                    // Inline diagnostic virtual text after a one-cell gap past the
                    // line's end (never on a `~` end-of-buffer filler row). The
                    // server already prefixed the message; the client positions and
                    // colors it, truncated to the remaining window width.
                    let is_filler = matches!(win.numbers.get(i), Some(None));
                    if !is_filler {
                        if let Some(Some((text, severity, id))) = win.diagnostics_virt.get(i) {
                            // The line's painted width includes the spliced inlay
                            // cells, so the virtual text sits past them too.
                            let inserted = inlay_shift(inlay, win.leftcol, u16::MAX, true);
                            let painted =
                                display.chars().count().saturating_sub(win.leftcol as usize)
                                    + inserted as usize;
                            let start = text_x0 + painted as u16 + 1;
                            let limit = (ox + wcols).saturating_sub(start);
                            if limit > 0 {
                                let shown: String = text.chars().take(limit as usize).collect();
                                let color = diag_color(&view.styles, *id, *severity, false);
                                self.push_plain(
                                    items,
                                    &shown,
                                    self.cell_px(start, row as u16),
                                    color,
                                    full,
                                );
                            }
                        }

                        // Extmark end-of-line / right-aligned `virt_text`. The painted
                        // width past which they sit includes the inlay hints **and** the
                        // inline `virt_text` spliced into the row (the same shifts the
                        // cursor takes). eol chunks paint after a one-cell gap; a
                        // right_align run flushes to the window's right edge, clamped to
                        // never overlap the painted text.
                        if !vtext.is_empty() {
                            let right_px = (ox + wcols) as f32 * self.cell_w;
                            let inlay_inserted = inlay_shift(inlay, win.leftcol, u16::MAX, true);
                            let virt_inserted = virt_inline_shift(vtext, win.leftcol, u16::MAX);
                            let painted =
                                (display.chars().count().saturating_sub(win.leftcol as usize)
                                    + inlay_inserted as usize
                                    + virt_inserted as usize)
                                    as u16;
                            // eol: one gap, then each placement's chunks in order.
                            let mut x = text_x0 + painted + 1;
                            for p in vtext.iter().filter(|p| p.pos == VIRT_POS_EOL) {
                                for (t, id) in &p.chunks {
                                    let limit = (ox + wcols).saturating_sub(x);
                                    if limit == 0 {
                                        break;
                                    }
                                    let shown: String = t.chars().take(limit as usize).collect();
                                    if shown.is_empty() {
                                        break;
                                    }
                                    let seg = virt_chunk_seg(&shown, *id, &view.styles, fg);
                                    self.push_seg_backgrounds(
                                        quads,
                                        std::slice::from_ref(&seg),
                                        x,
                                        row,
                                        right_px,
                                    );
                                    self.push_text(
                                        items,
                                        &[seg],
                                        self.cell_px(x, row as u16),
                                        fg,
                                        full,
                                    );
                                    x += shown.chars().count() as u16;
                                }
                            }
                            // right_align: stacked chunks flushed to the right edge.
                            let ra: Vec<&VirtChunk> = vtext
                                .iter()
                                .filter(|p| p.pos == VIRT_POS_RIGHT_ALIGN)
                                .flat_map(|p| p.chunks.iter())
                                .collect();
                            if !ra.is_empty() {
                                let total: u16 =
                                    ra.iter().map(|(t, _)| t.chars().count() as u16).sum();
                                // Start no earlier than the painted text (left-justify +
                                // truncate if the row is already full), no later than the
                                // right edge.
                                let mut rx = (ox + wcols)
                                    .saturating_sub(total)
                                    .max(text_x0 + painted + 1);
                                for (t, id) in ra {
                                    let limit = (ox + wcols).saturating_sub(rx);
                                    if limit == 0 {
                                        break;
                                    }
                                    let shown: String = t.chars().take(limit as usize).collect();
                                    if shown.is_empty() {
                                        break;
                                    }
                                    let seg = virt_chunk_seg(&shown, *id, &view.styles, fg);
                                    self.push_seg_backgrounds(
                                        quads,
                                        std::slice::from_ref(&seg),
                                        rx,
                                        row,
                                        right_px,
                                    );
                                    self.push_text(
                                        items,
                                        &[seg],
                                        self.cell_px(rx, row as u16),
                                        fg,
                                        full,
                                    );
                                    rx += shown.chars().count() as u16;
                                }
                            }
                        }
                    }
                }

                // Secondary multi-cursors over the settled text (a static-state
                // decoration like search; skipped mid-slide, where interpolated
                // positions wouldn't line up). The active primary cursor is recolored
                // below via `cursor_color`.
                self.paint_secondary_cursors(quads, win, view, text_x0, oy, wcols, text_rows);
            }
        }

        // Status bar on the window's bottom row (always from the live view — it
        // does not slide), painted with its `%`-format segments' own styles.
        if win.status_visible && (oy + wrows) as usize > 0 {
            let srow = oy + wrows.saturating_sub(1);
            self.build_status_row(&win.status, (ox, srow), wcols, view, quads, items);
        }

        // The cursor lives only in the focused window — but a focused panel, the
        // command line, or an open picker owns it instead, so suppress the window
        // cursor while any is active (it reappears in that widget). While sliding it
        // tracks the interpolated cursor line so it moves with the text.
        let picker_open = view.menu.as_ref().is_some_and(|m| m.query.is_some());
        if win.focused && !view.command_mode && !picker_open && text_rows > 0 {
            // The cursor shifts right by the inlay hints spliced in at or before its
            // column (a hint exactly at the cursor sits before the cursor glyph), so
            // it tracks the splice. Mid-slide that's the band's hints on the cursor's
            // band row; settled it's the window's hints on the cursor row.
            let cur_shift = match scroll {
                None => {
                    let row_inlay = win
                        .inlay_hints
                        .get(win.cursor_row as usize)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    // Inline `virt_text` spliced at or before the cursor pushes it right
                    // too — the GUI analogue of the TUI's `virt_cursor_shift`.
                    let row_vtext = win
                        .virt_text
                        .get(win.cursor_row as usize)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    inlay_shift(row_inlay, win.leftcol, win.cursor_screen_col, true)
                        + virt_inline_shift(row_vtext, win.leftcol, win.cursor_screen_col)
                }
                Some(s) => {
                    let idx = s.cursor_row.round() as usize; // cursor's band-row index
                    let row_inlay = s.inlay_hints.get(idx).map(Vec::as_slice).unwrap_or(&[]);
                    inlay_shift(row_inlay, win.leftcol, win.cursor_screen_col, true)
                }
            };
            // Clamp to the window's text area so a tiny window (a dock dragged to its
            // minimum, or a split squeezed to a few cells) never paints the cursor on
            // its status row or over the next region — quads aren't scissored, so the
            // clamp is what keeps it in bounds (mirrors the TUI's cursor clamp).
            let cx = (col_to_screen(text_x0, win.cursor_screen_col, win.leftcol) + cur_shift)
                .min((ox + wcols).saturating_sub(1));
            let px = cx as f32 * self.cell_w;
            let last_row = text_rows.saturating_sub(1);
            let py = match scroll {
                Some(s) => {
                    // The cursor's screen row within the viewport: its band row minus
                    // the interpolated viewport-top offset.
                    let r = (s.cursor_row - s.row_off).clamp(0.0, last_row as f32);
                    (oy as f32 + r) * self.cell_h
                }
                None => (oy + win.cursor_row.min(last_row)) as f32 * self.cell_h,
            };
            // Anchor the IME candidate window at the caret (read after the frame).
            self.cursor_px = Some((px, py));
            // In MultiCursor placement mode the active (primary) cursor wears the
            // multi-cursor accent, distinct from the secondaries, so it reads as
            // "the one dropping cursors" — mirroring the TUI.
            let cursor_color = if view.is_multicursor() {
                MULTICURSOR_ACCENT
            } else {
                style_fg(&view.normal).unwrap_or(DEFAULT_FG)
            };
            // A block cursor envelops the full display width of the grapheme it
            // sits on — a wide CJK/emoji glyph, or a `^X` / `<xx>` control token —
            // clamped to the window's right edge (quads aren't scissored). The
            // insert bar stays thin. Mirrors the TUI's enveloping block cursor.
            let block_cells = (win.cursor_width as usize)
                .min((ox + wcols).saturating_sub(cx).max(1) as usize)
                as f32;
            let (w, alpha) = if view.is_insert() {
                (self.cell_w * 0.15, 0.9)
            } else {
                (self.cell_w * block_cells, 0.5) // block, translucent so glyphs show
            };
            let c = srgb_to_color_rgba(cursor_color, alpha);
            // Replace mode → underline-ish thin block at the bottom.
            if view.is_replace() {
                let h = self.cell_h * 0.15;
                quads.push(Quad {
                    x: px,
                    y: py + self.cell_h - h,
                    w: self.cell_w,
                    h,
                    color: c,
                });
                return;
            }
            quads.push(Quad {
                x: px,
                y: py,
                w,
                h: self.cell_h,
                color: c,
            });
        }
    }

    /// Paint each secondary multi-cursor as a quad over the settled text, in the
    /// same mode-driven *shape* as the primary cursor — and, crucially, with the
    /// GUI's own shapes rather than the TUI's single-cell approximations: insert is
    /// a thin **bar** (the TUI falls back to an underline because it can't paint a
    /// bar in one cell, but the GPU can), and replace is a bottom underline — both
    /// accent-tinted so they read as the multi-cursor family, distinct from the
    /// text. Normal/visual is a half-transparent foreground block (reverse-video's
    /// GUI analogue), left un-accented so a placed cursor stays distinct from the
    /// accent-colored *active* primary in placement mode. Positions off the
    /// horizontal scroll or past the text edges are dropped, matching the primary's
    /// clamp. `oy`/`text_rows`/`wcols` bound the text area.
    #[allow(clippy::too_many_arguments)]
    fn paint_secondary_cursors(
        &self,
        quads: &mut Vec<Quad>,
        win: &WindowView,
        view: &View,
        text_x0: u16,
        oy: u16,
        wcols: u16,
        text_rows: u16,
    ) {
        let accent = color_to_rgba(srgb_to_color(MULTICURSOR_ACCENT));
        let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
        for &(crow, ccol) in &win.secondary_cursors {
            let Some(rel) = ccol.checked_sub(win.leftcol) else {
                continue; // scrolled off to the left
            };
            // Shift past the inlay hints spliced in at or before this cursor's
            // column on its row, matching the primary cursor and the text splice.
            let row_inlay = win
                .inlay_hints
                .get(crow as usize)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let x = text_x0 + rel + inlay_shift(row_inlay, win.leftcol, ccol, true);
            // Drop anything past the text area's right edge or below its last row.
            if crow >= text_rows || x >= text_x0.saturating_add(wcols) {
                continue;
            }
            let (px, py) = self.cell_px(x, oy + crow);
            if view.is_insert() {
                // Insert → a thin vertical bar, like the primary insert cursor.
                let w = (self.cell_w * 0.15).max(1.0);
                quads.push(Quad {
                    x: px,
                    y: py,
                    w,
                    h: self.cell_h,
                    color: accent,
                });
            } else if view.is_replace() {
                // Replace → a bottom underline, like the primary replace cursor.
                let h = (self.cell_h * 0.15).max(1.0);
                quads.push(Quad {
                    x: px,
                    y: py + self.cell_h - h,
                    w: self.cell_w,
                    h,
                    color: accent,
                });
            } else {
                // Normal/visual → a half-transparent foreground block (the glyph
                // shows through), the GUI analogue of reverse-video.
                let c = srgb_to_color_rgba(fg, 0.5);
                quads.push(Quad {
                    x: px,
                    y: py,
                    w: self.cell_w,
                    h: self.cell_h,
                    color: c,
                });
            }
        }
    }

    /// The command line (`:`/`/`/`?` or a `vim.ui.input` prompt) or, when idle,
    /// the message line — on the reserved bottom row. In command mode the editing
    /// cursor sits here (the window/panel cursor is suppressed), placed past the
    /// leading prompt at `cmdline_cursor` so it follows `<Left>`/`<Right>` edits.
    fn build_cmdline(
        &mut self,
        view: &View,
        cmd_row: u16,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let pos = self.cell_px(0, cmd_row);
        // Idle, with docks collapsed: advertise them as `▸{label}` chips (the click
        // affordance core's `hidden_chip_at` maps back). Geometry must match: from
        // col 0, each `▸{label}`, space-separated. A message / typed command wins.
        let (text, fg) = if view.command_mode || !view.cmdline.is_empty() {
            let line = if !view.cmdline_prompt.is_empty() {
                format!("{}{}", view.cmdline_prompt, view.cmdline)
            } else {
                format!("{}{}", view.cmdline_prefix, view.cmdline)
            };
            (
                line,
                style_fg(&view.msg_area)
                    .or_else(|| style_fg(&view.normal))
                    .unwrap_or(DEFAULT_FG),
            )
        } else if !view.message.is_empty() {
            // An error message paints with the theme's `ErrorMsg` foreground (a plain
            // red when the colorscheme leaves it undefined); else the default fg.
            let fg = if view.message_error {
                style_fg(&view.error_msg).unwrap_or(0xff_55_55)
            } else {
                style_fg(&view.normal).unwrap_or(DEFAULT_FG)
            };
            (view.message.clone(), fg)
        } else if !view.hidden_docks.is_empty() {
            let chips = view
                .hidden_docks
                .iter()
                .map(|l| format!("▸{l}"))
                .collect::<Vec<_>>()
                .join(" ");
            (chips, style_fg(&view.status_line).unwrap_or(DEFAULT_FG))
        } else {
            (String::new(), style_fg(&view.normal).unwrap_or(DEFAULT_FG))
        };
        let full = self.full_bounds();
        self.push_plain(items, &text, pos, fg, full);

        // The command-line cursor: a semi-transparent block past the leading prompt
        // (a single prefix char, or the multi-char `vim.ui.input` label). The caret
        // cell is display-width based — `cmdline_cursor` is a char offset, and a
        // wide CJK/emoji char in the prompt or the typed text occupies two cells of
        // the shaped run (see [`cmdline_caret_col`]).
        if view.command_mode {
            let col = cmdline_caret_col(&view.cmdline_prompt, &view.cmdline, view.cmdline_cursor);
            let (px, py) = self.cell_px(col, cmd_row);
            // Composing accented/CJK text in the command line or search: anchor the
            // IME candidate window at the command-line caret, not the window origin.
            self.cursor_px = Some((px, py));
            let cursor_color = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
            let c = srgb_to_color_rgba(cursor_color, 0.5);
            quads.push(Quad {
                x: px,
                y: py,
                w: self.cell_w,
                h: self.cell_h,
                color: c,
            });
        }
    }

    /// Paint the tabline on the top row: a custom `'tabline'`'s pre-rendered
    /// segments when set, else the built-in cells (` {count} {label}{+} `) themed
    /// from `TabLine`/`TabLineSel`/`TabLineFill` — the GUI port of the TUI's
    /// `render_tabline`.
    fn build_tabline(
        &mut self,
        view: &View,
        cols: u16,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let colors = TablineColors::resolve(view);
        self.fill_row(quads, 0, cols, colors.fill_bg);

        // A custom `'tabline'` already rendered to styled segments: paint verbatim.
        if !view.tabline.is_empty() && !view.tabline_segments.is_empty() {
            self.paint_segments(
                &view.tabline_segments,
                0,
                0,
                colors.inactive_fg,
                quads,
                items,
            );
            return;
        }

        self.build_tab_cells(
            "",
            &view.tabline,
            view.current_tab,
            0,
            0,
            cols,
            &colors,
            quads,
            items,
        );
    }

    /// Paint built-in tabline cells (` {count} {label}{+} `) starting at cell
    /// `(x0, row)`, preceded by an optional `title` label (the `nx.dock` dock
    /// title). Inactive cells use `TabLine`, the active cell `TabLineSel`, each
    /// resolved (with status-line / reverse-video fallbacks) in `colors`. Shared by
    /// the global (main) tabline and each dock's own tabline ([`build_dock_tablines`]).
    #[allow(clippy::too_many_arguments)]
    fn build_tab_cells(
        &mut self,
        title: &str,
        tabs: &[TabData],
        current: usize,
        x0: u16,
        row: u16,
        right: u16,
        colors: &TablineColors,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        // Clip every cell glyph to the strip's `[x0, right)` span so a dock tabline
        // in a band squeezed narrower than its labels (a vertical dock dragged to its
        // minimum) truncates at the band edge instead of bleeding over the separator
        // and the main area. For the full-width main tabline this is a no-op.
        let clip = self.text_bounds(x0, row, right.saturating_sub(x0), 1);
        let mut col = x0;
        if !title.is_empty() && col < right {
            let text = format!(" {title} ");
            let w = text.chars().count() as u16;
            let pos = self.cell_px(col, row);
            self.push_plain(items, &text, pos, colors.inactive_fg, clip);
            col = col.saturating_add(w);
        }
        for (i, tab) in tabs.iter().enumerate() {
            if col >= right {
                break; // ran off the end of the strip — the rest is clipped away
            }
            let count = if tab.window_count > 1 {
                format!("{} ", tab.window_count)
            } else {
                String::new()
            };
            let modified = if tab.modified { "+" } else { "" };
            let text = format!(" {count}{}{modified} ", tab.label);
            let w = text.chars().count() as u16;
            // A cell's background is a quad (unscissored), so clamp its width to the
            // strip edge. The active cell always fills (`TabLineSel`); an inactive
            // cell fills only when `TabLine` carries its own bg, else the bar fill
            // shows through.
            let (fg, cell_bg) = if i == current {
                (colors.active_fg, Some(colors.active_bg))
            } else {
                (colors.inactive_fg, colors.inactive_bg)
            };
            if let Some(bg) = cell_bg {
                let fill_w = w.min(right.saturating_sub(col));
                if fill_w > 0 {
                    self.fill_cells(quads, col, row, fill_w, bg);
                }
            }
            let pos = self.cell_px(col, row);
            self.push_plain(items, &text, pos, fg, clip);
            col = col.saturating_add(w);
        }
    }

    /// Paint each open dock's own tabline into its band's first row (the row the
    /// dock window content was shifted down past). `bands` carries each dock's
    /// `(x0, row, width, present)`; only present docks paint.
    fn build_dock_tablines(
        &mut self,
        view: &View,
        bands: &[(u16, u16, u16, &nxvim_view::RegionTabline, bool)],
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let colors = TablineColors::resolve(view);
        for &(x0, row, width, rt, present) in bands {
            if present && !rt.tabs.is_empty() {
                self.fill_cells(quads, x0, row, width, colors.fill_bg);
                self.build_tab_cells(
                    &rt.title,
                    &rt.tabs,
                    rt.current,
                    x0,
                    row,
                    x0 + width,
                    &colors,
                    quads,
                    items,
                );
            }
        }
    }

    /// Paint a status row (per-window or the global `laststatus=3` line) from the
    /// server's `%`-format segments. The base look is the theme's `StatusLine`
    /// (reverse-ish grey out of the box); each segment's own style patches its
    /// foreground (and background, when set) on top. Mirrors the TUI's
    /// `render_status`.
    fn build_status_row(
        &mut self,
        segments: &[StatusSegment],
        at: (u16, u16),
        width: u16,
        view: &View,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let (ox, row) = at;
        let base_bg = style_bg(&view.status_line).unwrap_or(0x2a_2a_2a);
        let base_fg = style_fg(&view.status_line).unwrap_or(DEFAULT_FG);
        // The base bar fills the whole row; segments paint over it.
        self.fill_cells(quads, ox, row, width, base_bg);
        self.paint_segments(segments, ox, row, base_fg, quads, items);
    }

    /// Paint a run of `%`-format `segments` left-to-right starting at cell
    /// `(ox, row)`: each segment's own background (when set) as a quad, then its
    /// text in its own foreground (falling back to `base_fg`). The char count is the
    /// cell advance. Glyphs are **not** clipped to their segment — they render fully,
    /// like the body text: an over-wide off-grid glyph (a powerline separator, a Nerd
    /// Font icon) is masked out of the inline run and redrawn as its own placed, scaled
    /// item by [`Renderer::push_text`], so it fills its slot (and overlaps into the
    /// neighbour like a powerline separator should) instead of being cut at the cell
    /// edge. Shared by the tabline and the status rows.
    fn paint_segments(
        &mut self,
        segments: &[StatusSegment],
        ox: u16,
        row: u16,
        base_fg: u32,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let mut col = ox;
        let full = self.full_bounds();
        for (text, style) in segments {
            let w = text.chars().count() as u16;
            if let Some(bg) = style.as_ref().and_then(|s| s.bg) {
                if w > 0 {
                    self.fill_cells(quads, col, row, w, bg);
                }
            }
            let fg = style.as_ref().and_then(|s| s.fg).unwrap_or(base_fg);
            let pos = self.cell_px(col, row);
            self.push_plain(items, text, pos, fg, full);
            col = col.saturating_add(w);
        }
    }

    /// Paint a float: an opaque background over its rect (so the tiled windows
    /// beneath don't bleed through), its border + title when bordered, then its
    /// content one cell inside via [`build_window`]. The GUI port of the TUI's
    /// float pass; the server already sorted floats by zindex.
    fn build_float(
        &mut self,
        view: &View,
        win: &WindowView,
        origin: (u16, u16),
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
        image_draws: &mut Vec<ImageDraw>,
    ) {
        let Some(r) = win.rect else {
            return; // a float always carries a rect
        };
        let ox = origin.0 + r.x;
        let oy = origin.1 + r.y;
        // Clip every glyph an earlier (lower-z) float already queued so this float's
        // opaque fill hides it — floats share one overlay layer that draws all
        // backgrounds before all glyphs, so a lower float's text would otherwise
        // bleed through this one's background (stacked floats read as "mixed").
        Self::occlude_overlay_text(items, self.text_bounds(ox, oy, r.width, r.height));
        // Prefer the colorscheme's float chrome (`NormalFloat` bg) when defined,
        // else the historical fallback: a slightly lightened `Normal` background.
        let bg = float_bg(view, 0x10);
        // Opaque fill over the whole float rect.
        let (px, py) = self.cell_px(ox, oy);
        quads.push(Quad {
            x: px,
            y: py,
            w: self.cell_w * r.width as f32,
            h: self.cell_h * r.height as f32,
            color: color_to_rgba(srgb_to_color(bg)),
        });

        let inner = match win.border {
            Some(border) => {
                // `FloatBorder` / `FloatTitle` foregrounds when the theme defines
                // them; else the historical fallback derived from the float bg.
                let border_fg = style_fg(&view.float_border).unwrap_or_else(|| lighten(bg, 0x30));
                let title_fg = style_fg(&view.float_title)
                    .or_else(|| style_fg(&view.normal))
                    .unwrap_or(DEFAULT_FG);
                self.draw_float_border(
                    items,
                    border,
                    win.title.as_deref(),
                    ox,
                    oy,
                    r.width,
                    r.height,
                    title_fg,
                    border_fg,
                );
                (
                    ox + 1,
                    oy + 1,
                    r.width.saturating_sub(2),
                    r.height.saturating_sub(2),
                )
            }
            None => (ox, oy, r.width, r.height),
        };
        // `'padding'` insets the content a further per-side margin inside any border.
        let inner = pad_rect(inner, win.padding);
        self.build_window(view, win, None, inner, quads, items, image_draws);
    }

    /// Draw a box border as box-drawing glyphs (not quad rules) — the single shared
    /// border path for EVERY bordered popup (completion pmenu, doc preview,
    /// picker/select, wildmenu, content + window floats), so they all look identical and
    /// match the TUI. The border *style* reads (rounded corners actually look rounded),
    /// and the title rides the top edge like the TUI's `title_top`. Glyph sets mirror the
    /// TUI's `BorderType` mapping (single / rounded / double / solid). `top`/`bottom` may
    /// be omitted for the flush look (the completion popup drops its top edge, the cmdline
    /// wildmenu its bottom). The title is left-aligned after the top-left corner, drawn in
    /// `title_fg` while the frame is drawn in `border_fg`.
    #[allow(clippy::too_many_arguments)]
    fn draw_glyph_border(
        &mut self,
        items: &mut Vec<TextItem>,
        border: Border,
        title: Option<&str>,
        ox: u16,
        oy: u16,
        w: u16,
        h: u16,
        top: bool,
        bottom: bool,
        title_fg: u32,
        border_fg: u32,
        center_title: bool,
    ) {
        if w < 2 || h < 1 {
            return; // no room for left + right rails
        }
        let (tl, tr, bl, br, horiz, vert) = match border {
            Border::Single => ('┌', '┐', '└', '┘', '─', '│'),
            Border::Rounded => ('╭', '╮', '╰', '╯', '─', '│'),
            Border::Double => ('╔', '╗', '╚', '╝', '═', '║'),
            Border::Solid => ('█', '█', '█', '█', '█', '█'),
        };
        let inner_w = (w - 2) as usize;
        let full = self.full_bounds();
        // Pure pixel-position helper that borrows the cell metrics by copy, so it
        // doesn't hold a `&self` borrow across the `&mut self` push calls below.
        let (cw, ch) = (self.cell_w, self.cell_h);
        let px = |col: u16, row: u16| (col as f32 * cw, row as f32 * ch);

        // Top edge: left corner, the title (truncated), a horizontal fill, then the
        // right corner — title in `title_fg`, the frame in `border_fg`. `center_title`
        // splits the fill on both sides of the title (the picker box); otherwise the
        // title is left-aligned (floats).
        if top {
            let title_s = title.map(|t| format!(" {t} ").chars().take(inner_w).collect::<String>());
            let title_len = title_s.as_deref().map_or(0, |s| s.chars().count());
            let total_fill = inner_w - title_len;
            let lfill = if center_title { total_fill / 2 } else { 0 };
            let rfill = total_fill - lfill;
            let mut top_segs = vec![Seg::plain(
                format!("{tl}{}", horiz.to_string().repeat(lfill)),
                border_fg,
            )];
            if let Some(ts) = title_s {
                top_segs.push(Seg::plain(ts, title_fg));
            }
            top_segs.push(Seg::plain(
                format!("{}{tr}", horiz.to_string().repeat(rfill)),
                border_fg,
            ));
            self.push_text(items, &top_segs, px(ox, oy), border_fg, full);
        }
        if bottom {
            let bottom_s = format!("{bl}{}{br}", horiz.to_string().repeat(inner_w));
            self.push_plain(items, &bottom_s, px(ox, oy + h - 1), border_fg, full);
        }
        // The two side rails span the rows between whichever edges are present.
        let vert_s = vert.to_string();
        let rail_start = oy + u16::from(top);
        let rail_end = oy + h - u16::from(bottom);
        for row in rail_start..rail_end {
            self.push_plain(items, &vert_s, px(ox, row), border_fg, full);
            self.push_plain(items, &vert_s, px(ox + w - 1, row), border_fg, full);
        }
    }

    /// A full glyph ring (all four edges) — the float case of [`Self::draw_glyph_border`].
    #[allow(clippy::too_many_arguments)]
    fn draw_float_border(
        &mut self,
        items: &mut Vec<TextItem>,
        border: Border,
        title: Option<&str>,
        ox: u16,
        oy: u16,
        w: u16,
        h: u16,
        title_fg: u32,
        border_fg: u32,
    ) {
        self.draw_glyph_border(
            items, border, title, ox, oy, w, h, true, true, title_fg, border_fg, false,
        );
    }

    /// A vertical glyph separator (`│`) `h` cells tall at `(x, y)` in `border_fg` — the
    /// divider between a picker's list column and its preview pane.
    fn draw_glyph_vrule(
        &mut self,
        items: &mut Vec<TextItem>,
        x: u16,
        y: u16,
        h: u16,
        border_fg: u32,
    ) {
        let full = self.full_bounds();
        for row in y..y + h {
            self.push_plain(items, "│", self.cell_px(x, row), border_fg, full);
        }
    }

    /// Fill a `w`×`h`-cell rect at `(x, y)` with `bg` (no border) — the opaque backing
    /// behind a glyph-bordered popup (the glyph border draws over it).
    fn fill_rect(&self, quads: &mut Vec<Quad>, x: u16, y: u16, w: u16, h: u16, bg: u32) {
        let (px, py) = self.cell_px(x, y);
        quads.push(Quad {
            x: px,
            y: py,
            w: self.cell_w * w as f32,
            h: self.cell_h * h as f32,
            color: color_to_rgba(srgb_to_color(bg)),
        });
    }

    /// Paint the insert-mode completion popup over the focused window's text area:
    /// a bordered box anchored under the completion word (past the gutter), each
    /// item on its own row with the selected one reverse-highlighted, and the
    /// selected item's docs in a preview box beside it. The GUI port of the TUI's
    /// `render_pmenu`; the doc preview scrolls by `doc_scroll` (a client-side mouse
    /// wheel gesture, like the TUI's).
    fn build_pmenu(
        &mut self,
        view: &View,
        origin: (u16, u16),
        doc_scroll: u16,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let Some(pmenu) = &view.pmenu else {
            return;
        };
        let Some(win) = view.focused() else {
            return;
        };
        // The focused window's text-inner origin in screen cells: its rect (offset
        // by the windows-area origin, inset past a float's border) plus its gutter.
        let (mut wx, mut wy) = match win.rect {
            Some(r) => (origin.0 + r.x, origin.1 + r.y),
            None => (origin.0, origin.1),
        };
        if win.floating && win.border.is_some() {
            wx += 1;
            wy += 1;
        }
        // `'padding'` insets the text body, so the popup anchors a margin in too.
        wx += win.padding.left;
        wy += win.padding.top;
        let sign_w = win.sign_width;
        let gutter = if win.number || win.relativenumber {
            win.number_width
        } else {
            0
        };
        let text_x0 = wx + win.foldcolumn_width + sign_w + gutter;

        let cols = self.grid_size().0;
        let popup_bg = lighten(style_bg(&view.normal).unwrap_or(DEFAULT_BG), 0x14);
        let border = lighten(popup_bg, 0x30);
        let sel_bg = lighten(popup_bg, 0x28);
        let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);

        // Box: content `width`×`height` plus a one-cell border ring, anchored under
        // the completion word.
        let bx = text_x0 + pmenu.col;
        let by = wy + pmenu.row;
        let box_w = pmenu.width + 2;
        let box_h = pmenu.height + 2;
        self.fill_rect(quads, bx, by, box_w, box_h, popup_bg);
        self.draw_glyph_border(
            items,
            Border::Single,
            None,
            bx,
            by,
            box_w,
            box_h,
            true,
            true,
            fg,
            border,
            false,
        );

        let rows = pmenu.height as usize;
        let start = pmenu_start(pmenu.selected, rows);
        let full = self.full_bounds();
        for r in 0..pmenu.height {
            let idx = start + r as usize;
            let Some((label, _kind, detail)) = pmenu.items.get(idx) else {
                continue;
            };
            let cx = bx + 1;
            let row = by + 1 + r;
            let selected = Some(idx) == pmenu.selected;
            if selected {
                self.fill_cells(quads, cx, row, pmenu.width, sel_bg);
            }
            let text = pmenu_row(label, detail, pmenu.width as usize);
            self.push_plain(items, &text, self.cell_px(cx, row), fg, full);
        }

        // The selected item's documentation preview, beside the popup.
        if !pmenu.doc.is_empty() {
            const MAX_W: u16 = 50;
            const MAX_H: u16 = 12;
            let natural = pmenu
                .doc
                .iter()
                .map(|l| l.chars().count())
                .max()
                .unwrap_or(0) as u16;
            let dcw = natural.clamp(1, MAX_W);
            let dch = (pmenu.doc.len() as u16).clamp(1, MAX_H);
            let dbw = dcw + 2;
            let dbh = dch + 2;
            // Prefer the right of the popup; fall back to its left when there's no
            // room (vim's `completeopt=popup` shape), top-aligned with the popup.
            let dx = if bx + box_w + dbw <= cols {
                bx + box_w
            } else {
                bx.saturating_sub(dbw)
            };
            self.fill_rect(quads, dx, by, dbw, dbh, popup_bg);
            self.draw_glyph_border(
                items,
                Border::Single,
                None,
                dx,
                by,
                dbw,
                dbh,
                true,
                true,
                fg,
                border,
                false,
            );
            // Client-side vertical scroll (the box height is the client's to know,
            // so the server has no notion of it): skip the scrolled-past lines,
            // clamped so the wheel can't overscroll past the last screenful.
            let max_scroll = pmenu.doc.len().saturating_sub(dch as usize);
            let skip = (doc_scroll as usize).min(max_scroll);
            for (r, line) in pmenu.doc.iter().skip(skip).take(dch as usize).enumerate() {
                let text: String = line.chars().take(dcw as usize).collect();
                let pos = self.cell_px(dx + 1, by + 1 + r as u16);
                self.push_plain(items, &text, pos, fg, full);
            }
        }
    }

    /// Paint the floating selectable-list menu (`nx.ui.select`) — the same opaque
    /// bordered overlay as the completion popup, anchored over the focused window,
    /// but each row is a plain label and the highlighted row gets the selection
    /// fill. Mirrors [`Self::build_pmenu`] (without the doc preview).
    fn build_menu(
        &mut self,
        view: &View,
        origin: (u16, u16),
        cmd_row: u16,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let Some(menu) = &view.menu else {
            return;
        };
        // The anchor base for the box. The `nx.picker` overlay (`editor_relative`) is
        // editor-absolute (windows-area cells), so it anchors at the grid origin and
        // floats over the whole editor — a split can't drag it into the focused pane;
        // every other menu anchors to the focused window's text-inner origin (rect +
        // padding + gutter), identical to the popup's anchor derivation.
        let (text_x0, wy) = if menu.editor_relative {
            (0, 0)
        } else {
            let Some(win) = view.focused() else {
                return;
            };
            let (mut wx, mut wy) = match win.rect {
                Some(r) => (origin.0 + r.x, origin.1 + r.y),
                None => (origin.0, origin.1),
            };
            if win.floating && win.border.is_some() {
                wx += 1;
                wy += 1;
            }
            // `'padding'` insets the text body, so the popup anchors a margin in too.
            wx += win.padding.left;
            wy += win.padding.top;
            let sign_w = win.sign_width;
            let gutter = if win.number || win.relativenumber {
                win.number_width
            } else {
                0
            };
            (wx + sign_w + gutter, wy)
        };

        // Themed colors (nvim-cmp / telescope groups, resolved server-side), each
        // falling back to the built-in derived look when its group is undefined: the
        // popup bg + fg (`Pmenu` / `TelescopeNormal`), the border (`FloatBorder` /
        // `TelescopeBorder`), the selection (`PmenuSel` / `TelescopeSelection`), and
        // the matched-character accent (`CmpItemAbbrMatch` / `TelescopeMatching`).
        let popup_bg = style_bg(&menu.styles.bg)
            .unwrap_or_else(|| lighten(style_bg(&view.normal).unwrap_or(DEFAULT_BG), 0x14));
        let border = style_fg(&menu.styles.border).unwrap_or_else(|| lighten(popup_bg, 0x30));
        let sel_bg = style_bg(&menu.styles.sel).unwrap_or_else(|| lighten(popup_bg, 0x28));
        let fg = style_fg(&menu.styles.bg)
            .or_else(|| style_fg(&view.normal))
            .unwrap_or(DEFAULT_FG);
        let sel_fg = style_fg(&menu.styles.sel);
        let prompt_fg = style_fg(&menu.styles.prompt).unwrap_or(fg);

        // The completion popup drops its top border + top padding so it sits flush
        // against the line below the cursor, and shifts one cell left so the left
        // padding doesn't push the list off the word (`menu.col` is the content
        // anchor; the box origin sits one cell before it). `select` / picker are
        // fully bordered and anchored at `menu.col`.
        // The command-line wildmenu is anchored to the command line, not the focused
        // window: it floats just *above* the `cmd_row` (frame col 0, no gutter) and
        // drops its *bottom* border so the list sits flush against the input — the
        // mirror of the below-cursor completion popup (which drops its top border).
        let top_pad = u16::from(menu.border_top || menu.cmdline);
        let left_shift = u16::from(!menu.border_top && !menu.cmdline);
        let box_w = menu.width + 2;
        // Borders counted into the box height: a top edge for a bordered / cmdline box,
        // a bottom edge for everything except the cmdline wildmenu (flush to the input).
        let box_h = menu.height + top_pad + u16::from(!menu.cmdline);
        let (bx, by) = if menu.cmdline {
            (origin.0 + menu.col, cmd_row.saturating_sub(box_h))
        } else {
            (
                (text_x0 + menu.col).saturating_sub(left_shift),
                wy + menu.row,
            )
        };
        // Opaque backing + a single-line glyph border (the shared popup border path):
        // the cmdline wildmenu drops its bottom edge (flush to the input below), a
        // completion-style popup (`!border_top`) its top edge, a bordered select/picker
        // keeps all four.
        let (border_top, border_bottom) = (menu.border_top || menu.cmdline, !menu.cmdline);
        self.fill_rect(quads, bx, by, box_w, box_h, popup_bg);
        self.draw_glyph_border(
            items,
            Border::Single,
            // The picker box's title (`nx.picker.open{ title = … }`) on the top edge;
            // only a fully-bordered picker sets one (the wildmenu / completion don't).
            menu.title.as_deref().filter(|t| !t.is_empty()),
            bx,
            by,
            box_w,
            box_h,
            border_top,
            border_bottom,
            fg,
            border,
            true, // the picker box title is centered
        );

        let full = self.full_bounds();
        let cx = bx + 1;
        // The first content row: below the top border for a fully bordered box,
        // or flush at the box top for the top-borderless completion popup.
        let content_y0 = by + top_pad;
        // Split the box content into a list column (left) + a 1-col vertical
        // separator + a preview column (right) when the picker carries a preview
        // pane; otherwise the list spans the full content width. The prompt + results
        // live in the list column; the preview spans the full content height.
        let (list_w, preview_w) = match &menu.preview {
            Some(pv) => {
                let pw = pv.width.min(menu.width.saturating_sub(2)).max(1);
                (menu.width.saturating_sub(pw + 1).max(1), pw)
            }
            None => (menu.width, 0),
        };
        // A picker carries a prompt row plus a separator (the `chrome`); the list
        // fills the rest. The prompt sits above the list by default, or below it
        // (telescope-style) when asked. A promptless `nx.ui.select` has neither.
        let has_prompt = menu.query.is_some();
        let chrome = u16::from(has_prompt) * 2;
        let list_rows = menu.height.saturating_sub(chrome);
        // Row offsets within the box content (below the top border at `by + 1`): the
        // first list row, the prompt row, and the separator row.
        let (list_y0, prompt_y, sep_y) = if !has_prompt {
            (0, 0, 0)
        } else if menu.prompt_bottom {
            (0, list_rows + 1, list_rows)
        } else {
            (2, 0, 1)
        };

        if has_prompt {
            let query = menu.query.as_deref().unwrap_or("");
            let text = pmenu_row(&format!("> {query}"), "", list_w as usize);
            self.push_plain(
                items,
                &text,
                self.cell_px(cx, content_y0 + prompt_y),
                prompt_fg,
                full,
            );
            // The separator: a `─` glyph rule across the list column — drawn as box
            // glyphs (not a thin quad) so it matches the glyph box border, the `│`
            // preview rule, and the TUI/web clients.
            let full = self.full_bounds();
            self.push_plain(
                items,
                &"─".repeat(list_w as usize),
                self.cell_px(cx, content_y0 + sep_y),
                border,
                full,
            );
            // The caret: a thin bar past the `> ` prefix at the query cursor —
            // display-width based, since `query_cursor` is a char offset and a wide
            // CJK char in the query occupies two cells (see [`query_caret_col`]).
            let caret =
                query_caret_col(query, menu.query_cursor as usize).min(list_w.saturating_sub(1));
            let (cpx, cpy) = self.cell_px(cx + caret, content_y0 + prompt_y);
            let c = srgb_to_color_rgba(fg, 0.9);
            quads.push(Quad {
                x: cpx,
                y: cpy,
                w: self.cell_w * 0.15,
                h: self.cell_h,
                color: c,
            });
        }

        // Match-highlight overdraw iterates the *untruncated* label, so it must clip
        // to the list column or a label wider than `list_w` bleeds its matched chars
        // over the separator and preview pane (the base row text is already truncated
        // to `list_w` by `pmenu_row`, so it needs no such guard).
        let list_bounds = self.text_bounds(cx, content_y0, list_w, menu.height);
        // A noselect completion popup highlights no row and scrolls from the top.
        let sel = menu.selected_active.then_some(menu.selected);
        let start = pmenu_start(sel, list_rows as usize);
        // Matched characters use the theme's match group (`CmpItemAbbrMatch` /
        // `TelescopeMatching`) when defined, else a warm built-in accent.
        let match_fg = style_fg(&menu.styles.matched).unwrap_or(0x00E5_C07B);
        for r in 0..list_rows {
            let idx = start + r as usize;
            let Some(label) = menu.items.get(idx) else {
                continue;
            };
            // The command-line wildmenu floats above its input, so flip the list to
            // keep the best match (row 0) at the bottom, nearest the command cursor.
            let display_r = if menu.cmdline { list_rows - 1 - r } else { r };
            let row = content_y0 + list_y0 + display_r;
            // The selected row gets the selection background and (when the theme's
            // selection group carries one) its own foreground.
            let row_fg = if sel == Some(idx) {
                self.fill_cells(quads, cx, row, list_w, sel_bg);
                sel_fg.unwrap_or(fg)
            } else {
                fg
            };
            // Path-priority truncation: when the row overflows, keep the file name
            // (the path tail) on screen by dropping leading directory components
            // behind a `…`, rather than the head-cut `pmenu_row` would apply (which
            // hides the name). Rows that fit — and non-path rows — pass through; the
            // match spans are remapped onto the elided string to stay aligned.
            let empty = Vec::new();
            let spans = menu.match_spans.get(idx).unwrap_or(&empty);
            let (label, spans) = elide_keep_tail(label, spans, list_w as usize);
            let text = pmenu_row(&label, "", list_w as usize);
            self.push_plain(items, &text, self.cell_px(cx, row), row_fg, full);
            // Overdraw the matched characters in the accent color (monospace, so
            // char `i` sits at column `cx + i`).
            for (i, ch) in label.chars().enumerate() {
                let ci = i as u16;
                if spans.iter().any(|(s, e)| ci >= *s && ci < *e) {
                    self.push_plain(
                        items,
                        &ch.to_string(),
                        self.cell_px(cx + ci, row),
                        match_fg,
                        list_bounds,
                    );
                }
            }
        }

        // The preview column: a vertical separator rule, a header row with the file
        // path, then the windowed file lines — syntax-coloured from the server's
        // tree-sitter `highlights` (Phase 3b) via `row_segments`, the same colouring
        // the window text uses — with the match line (`loc`) background-highlighted.
        if let Some(pv) = &menu.preview {
            let sep_col = cx + list_w;
            let px0 = sep_col + 1;
            // Glyph `│` separator down the box content height (the shared border tint).
            self.draw_glyph_vrule(items, sep_col, content_y0, menu.height, border);
            // The title header: the path on a sel-tinted bar across the pane.
            self.fill_cells(quads, px0, content_y0, preview_w, sel_bg);
            let title = pmenu_row(
                &elide_middle(&pv.title, preview_w as usize),
                "",
                preview_w as usize,
            );
            self.push_plain(items, &title, self.cell_px(px0, content_y0), fg, full);
            // The windowed file lines below the header (rows 1..content height).
            let content_h = menu.height.saturating_sub(1);
            let empty = Vec::new();
            for (i, text) in pv.lines.iter().enumerate() {
                if i as u16 >= content_h {
                    break;
                }
                let row = content_y0 + 1 + i as u16;
                if pv.loc.is_some_and(|(r, _)| r as usize == i) {
                    self.fill_cells(quads, px0, row, preview_w, sel_bg);
                }
                // Colour each run by its tree-sitter span (char columns, no leftcol),
                // clamped to the pane width; a span with no theme id falls back to its
                // capture group's built-in colour (`row_segments`).
                let hl = pv.highlights.get(i).map(Vec::as_slice).unwrap_or(&empty);
                let mut col = 0u16;
                for seg in row_segments(text, hl, &view.styles, fg, popup_bg, 0) {
                    if col >= preview_w {
                        break;
                    }
                    let room = (preview_w - col) as usize;
                    let shown: String = seg.text.chars().take(room).collect();
                    if shown.is_empty() {
                        continue;
                    }
                    let n = shown.chars().count() as u16;
                    self.push_plain(items, &shown, self.cell_px(px0 + col, row), seg.fg, full);
                    col += n;
                }
            }
        }

        // (The completion / cmdline **docs** are no longer a `menu.docs` overlay — they
        // render as real doc-float windows through the normal window path, so there is
        // nothing to draw here.)
    }

    /// Build the list-less content float (`nx.ui.float`; LSP hover / signature
    /// help): a bordered box of plain content lines at the server-placed geometry,
    /// anchored at the focused window's text-inner origin (the same derivation as
    /// the popup / docs sidebar). No selection, no scrolling — the server already
    /// windowed the lines. A `None` border draws the content with no box.
    fn build_content_float(
        &mut self,
        view: &View,
        origin: (u16, u16),
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let Some(float) = &view.content_float else {
            return;
        };
        // A cursor-anchored float (hover / signature) sits over the focused window's
        // text area; an `editor`/`bottom`-relative float (the which-key surface)
        // anchors over the whole editor's windows area at the grid origin, matching
        // the server's geometry, so a split doesn't drag it into the focused pane.
        let (text_x0, wy) = if float.editor_relative {
            (0, 0)
        } else {
            let Some(win) = view.focused() else {
                return;
            };
            let (mut wx, mut wy) = match win.rect {
                Some(r) => (origin.0 + r.x, origin.1 + r.y),
                None => (origin.0, origin.1),
            };
            if win.floating && win.border.is_some() {
                wx += 1;
                wy += 1;
            }
            // `'padding'` insets the text body, so the popup anchors a margin in too.
            wx += win.padding.left;
            wy += win.padding.top;
            let sign_w = win.sign_width;
            let gutter = if win.number || win.relativenumber {
                win.number_width
            } else {
                0
            };
            (wx + sign_w + gutter, wy)
        };

        // Prefer the colorscheme's float chrome (`NormalFloat` / `FloatBorder` /
        // `FloatTitle`) when defined, else the historical `Normal`-derived fallback.
        let popup_bg = float_bg(view, 0x14);
        let border = style_fg(&view.float_border).unwrap_or_else(|| lighten(popup_bg, 0x30));
        let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
        let title_fg = style_fg(&view.float_title).unwrap_or(fg);
        let full = self.full_bounds();

        let (bx, by) = (text_x0 + float.col, wy + float.row);
        let (cx, cy) = if let Some(border_style) = float.border {
            let (bw, bh) = (float.width + 2, float.height + 2);
            // Opaque bg behind the whole box, then a glyph border (box-drawing chars)
            // so the border *style* reads — rounded corners look rounded — and the
            // title rides the top edge, integrated with the line like the TUI's
            // `title_top`. Same path as a window float (`draw_float_border`).
            let (px, py) = self.cell_px(bx, by);
            quads.push(Quad {
                x: px,
                y: py,
                w: self.cell_w * bw as f32,
                h: self.cell_h * bh as f32,
                color: color_to_rgba(srgb_to_color(popup_bg)),
            });
            self.draw_float_border(
                items,
                border_style,
                float.title.as_deref(),
                bx,
                by,
                bw,
                bh,
                title_fg,
                border,
            );
            (bx + 1, by + 1)
        } else {
            (bx, by)
        };
        let w = float.width as usize;
        for (i, line) in float.lines.iter().enumerate() {
            if i as u16 >= float.height {
                break;
            }
            // Each chunk paints its text in its resolved style (fg + bold/italic),
            // falling back to the popup's normal fg; a plain caller's single
            // un-styled chunk is just normal text. Pad the row to the box width so
            // the popup background fills it (matching the TUI's `pmenu_row`).
            let mut segs: Vec<Seg> = Vec::new();
            let mut chars = 0usize;
            for (text, id) in line {
                if chars >= w {
                    break;
                }
                let shown: String = text.chars().take(w - chars).collect();
                if shown.is_empty() {
                    continue;
                }
                chars += shown.chars().count();
                let st = id.and_then(|id| view.styles.get(id));
                segs.push(Seg {
                    fg: st.and_then(|s| s.fg).unwrap_or(fg),
                    bg: st.and_then(|s| s.bg),
                    bold: st.is_some_and(|s| s.bold),
                    italic: st.is_some_and(|s| s.italic),
                    text: shown,
                });
            }
            if chars < w {
                segs.push(Seg::plain(" ".repeat(w - chars), fg));
            }
            self.push_text(items, &segs, self.cell_px(cx, cy + i as u16), fg, full);
        }
    }

    /// Fill `count` cells of `row` from column `col` with `color`.
    fn fill_cells(&self, quads: &mut Vec<Quad>, col: u16, row: u16, count: u16, color: u32) {
        let (px, py) = self.cell_px(col, row);
        quads.push(Quad {
            x: px,
            y: py,
            w: self.cell_w * count as f32,
            h: self.cell_h,
            color: color_to_rgba(srgb_to_color(color)),
        });
    }

    /// Fill the whole of `row` (`cols` cells wide) with `color`.
    fn fill_row(&self, quads: &mut Vec<Quad>, row: u16, cols: u16, color: u32) {
        self.fill_cells(quads, 0, row, cols, color);
    }

    /// Resolve a buffer-column `span` to the on-screen cell range `[start, end)` it
    /// covers: anchored at column `base`, shifted left by `leftcol`, and pushed right
    /// past any inlay hints inserted before each endpoint. `None` when the span lands
    /// empty (or fully scrolled off the left), so a span-quad / underline / strike
    /// caller can early-out. Shared by [`Self::push_underline_at`],
    /// [`Self::push_strike`], [`Self::push_span_quad`], and [`Self::push_span_quad_at`].
    fn span_cols(
        &self,
        base: u16,
        span: (u16, u16),
        leftcol: u16,
        inlay: &[InlayHint],
    ) -> Option<(u16, u16)> {
        let (s, e) = span;
        let start = base + s.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, s, true);
        let end = base + e.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, e, false);
        (end > start).then_some((start, end))
    }

    /// Push a thin underline rule under cells `[s, e)` of `row` (a diagnostic
    /// underline / undercurl approximation), offset left by `leftcol` and anchored
    /// at column `base` like [`push_span_quad`].
    #[allow(clippy::too_many_arguments)]
    fn push_underline(
        &self,
        quads: &mut Vec<Quad>,
        base: u16,
        row: usize,
        span: (u16, u16),
        leftcol: u16,
        inlay: &[InlayHint],
        color: u32,
    ) {
        let (_, py) = self.cell_px(0, row as u16);
        self.push_underline_at(quads, base, py, span, leftcol, inlay, color);
    }

    /// [`push_underline`] at an explicit pixel `y` (the scroll band's interpolated
    /// row origin) rather than a whole cell row, so the squiggle slides with the
    /// band's sub-pixel offset.
    #[allow(clippy::too_many_arguments)]
    fn push_underline_at(
        &self,
        quads: &mut Vec<Quad>,
        base: u16,
        py: f32,
        span: (u16, u16),
        leftcol: u16,
        inlay: &[InlayHint],
        color: u32,
    ) {
        let Some((start, end)) = self.span_cols(base, span, leftcol, inlay) else {
            return;
        };
        let h = (self.cell_h * 0.08).max(1.0);
        quads.push(Quad {
            x: start as f32 * self.cell_w,
            y: py + self.cell_h - h,
            w: self.cell_w * (end - start) as f32,
            h,
            color: color_to_rgba(srgb_to_color(color)),
        });
    }

    /// Push a thin strikethrough rule through the vertical middle of cells
    /// `[s, e)` of `row` (a `strikethrough` style attribute), offset and anchored
    /// like [`push_underline`].
    #[allow(clippy::too_many_arguments)]
    fn push_strike(
        &self,
        quads: &mut Vec<Quad>,
        base: u16,
        row: usize,
        span: (u16, u16),
        leftcol: u16,
        inlay: &[InlayHint],
        color: u32,
    ) {
        let Some((start, end)) = self.span_cols(base, span, leftcol, inlay) else {
            return;
        };
        let (px, py) = self.cell_px(start, row as u16);
        let h = (self.cell_h * 0.08).max(1.0);
        quads.push(Quad {
            x: px,
            y: py + (self.cell_h - h) * 0.5,
            w: self.cell_w * (end - start) as f32,
            h,
            color: color_to_rgba(srgb_to_color(color)),
        });
    }

    /// Paint the reverse-video fills for a row: a foreground-colored quad behind
    /// each run whose style sets `reverse`. `row_segments` already recolored the
    /// glyph to the background, so quad + glyph together read inverted. Painted
    /// before the text, as every quad draws under the glyphs.
    #[allow(clippy::too_many_arguments)]
    fn push_reverse_fills(
        &self,
        quads: &mut Vec<Quad>,
        win: &WindowView,
        view: &View,
        text_x0: u16,
        row: usize,
        hl: &[nxvim_view::HlSpan],
        inlay: &[InlayHint],
    ) {
        let base_fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
        for hs in hl {
            if let Some(st) = hs.3.and_then(|id| view.styles.get(id)) {
                if st.reverse {
                    let color = st.fg.unwrap_or(base_fg);
                    self.push_span_quad(
                        quads,
                        text_x0,
                        row,
                        (hs.0, hs.1),
                        win.leftcol,
                        inlay,
                        color,
                    );
                }
            }
        }
    }

    /// Paint the over-text rules for a row: an underline (also standing in for
    /// undercurl) in the `sp`/`fg` color, and a strikethrough in the `fg` color,
    /// for each run whose style sets them. Drawn after the glyphs so the rule sits
    /// on top of the text.
    #[allow(clippy::too_many_arguments)]
    fn push_attr_rules(
        &self,
        quads: &mut Vec<Quad>,
        win: &WindowView,
        view: &View,
        text_x0: u16,
        row: usize,
        hl: &[nxvim_view::HlSpan],
        inlay: &[InlayHint],
    ) {
        let base_fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
        for hs in hl {
            let Some(st) = hs.3.and_then(|id| view.styles.get(id)) else {
                continue;
            };
            let span = (hs.0, hs.1);
            if st.underline || st.undercurl {
                let color = st.sp.or(st.fg).unwrap_or(base_fg);
                self.push_underline(quads, text_x0, row, span, win.leftcol, inlay, color);
            }
            if st.strikethrough {
                let color = st.fg.unwrap_or(base_fg);
                self.push_strike(quads, text_x0, row, span, win.leftcol, inlay, color);
            }
        }
    }

    /// Push a background quad covering screen columns `[s, e)` of `row`, offset
    /// left by `leftcol`, shifted right by the row's inlay hints (`inlay`), and
    /// anchored at column `base` (the text origin). The span's left edge clears the
    /// hints at or before `s` (a hint *at* `s` sits before the first highlighted
    /// glyph, so it stays unhighlighted); its right edge clears only the hints
    /// strictly before `e` (a hint *at* `e` is past the span).
    #[allow(clippy::too_many_arguments)]
    fn push_span_quad(
        &self,
        quads: &mut Vec<Quad>,
        base: u16,
        row: usize,
        span: (u16, u16),
        leftcol: u16,
        inlay: &[InlayHint],
        color: u32,
    ) {
        let Some((start, end)) = self.span_cols(base, span, leftcol, inlay) else {
            return;
        };
        let (px, py) = self.cell_px(start, row as u16);
        quads.push(Quad {
            x: px,
            y: py,
            w: self.cell_w * (end - start) as f32,
            h: self.cell_h,
            color: color_to_rgba(srgb_to_color(color)),
        });
    }

    /// Like [`push_span_quad`], but at an explicit fractional pixel `y` (a sliding
    /// row sits between cell rows) and vertically clamped to `[clip_top,
    /// clip_bottom]` — quads carry no scissor, so a partially-scrolled selection
    /// row must cut off at the text-area edge itself instead of bleeding over the
    /// status row or a neighbour.
    #[allow(clippy::too_many_arguments)]
    fn push_span_quad_at(
        &self,
        quads: &mut Vec<Quad>,
        base: u16,
        y: f32,
        span: (u16, u16),
        leftcol: u16,
        inlay: &[InlayHint],
        color: u32,
        clip_top: f32,
        clip_bottom: f32,
    ) {
        let Some((start, end)) = self.span_cols(base, span, leftcol, inlay) else {
            return;
        };
        let top = y.max(clip_top);
        let bottom = (y + self.cell_h).min(clip_bottom);
        if bottom <= top {
            return; // fully outside the text area
        }
        quads.push(Quad {
            x: start as f32 * self.cell_w,
            y: top,
            w: self.cell_w * (end - start) as f32,
            h: bottom - top,
            color: color_to_rgba(srgb_to_color(color)),
        });
    }

    /// Paint background quads behind any [`Seg`] carrying an explicit `bg` (extmark
    /// `virt_text` chunks whose highlight group sets one). `segments` are laid out
    /// contiguously from screen column `start_col`, so each bg run covers its own
    /// cells; the quad's right edge is clamped to `right_px` (the window's content
    /// edge) so a chunk near the edge doesn't bleed into the next split. Quads render
    /// under the glyphs, so the chunk reads as a filled badge rather than dark-on-dark.
    fn push_seg_backgrounds(
        &self,
        quads: &mut Vec<Quad>,
        segments: &[Seg],
        start_col: u16,
        row: usize,
        right_px: f32,
    ) {
        let mut col = start_col;
        for seg in segments {
            let w = seg.text.chars().count() as u16;
            if let Some(bg) = seg.bg {
                if w > 0 {
                    let (px, py) = self.cell_px(col, row as u16);
                    let right = (px + self.cell_w * w as f32).min(right_px);
                    if right > px {
                        quads.push(Quad {
                            x: px,
                            y: py,
                            w: right - px,
                            h: self.cell_h,
                            color: color_to_rgba(srgb_to_color(bg)),
                        });
                    }
                }
            }
            col += w;
        }
    }

    /// Pixel origin of cell `(col, row)`.
    fn cell_px(&self, col: u16, row: u16) -> (f32, f32) {
        (col as f32 * self.cell_w, row as f32 * self.cell_h)
    }

    /// Clip every glyph already queued in `items` so none of it renders inside the
    /// opaque pixel rect `hole` — the background of a float about to be drawn on
    /// top. Floats share one overlay layer whose pass draws *all* backgrounds and
    /// then *all* glyphs (see [`render`](Self::render)), so without this a lower
    /// float's text would show through a higher float's fill (stacked floats looked
    /// "mixed" / transparent). Each covered item is replaced by the remainder
    /// pieces of its clip rect outside `hole`, so text outside the overlap is
    /// untouched.
    fn occlude_overlay_text(items: &mut Vec<TextItem>, hole: TextBounds) {
        let h = (hole.left, hole.top, hole.right, hole.bottom);
        let mut out = Vec::with_capacity(items.len());
        for it in items.drain(..) {
            let a = (
                it.bounds.left,
                it.bounds.top,
                it.bounds.right,
                it.bounds.bottom,
            );
            for (left, top, right, bottom) in rect_subtract(a, h) {
                out.push(TextItem {
                    bounds: TextBounds {
                        left,
                        top,
                        right,
                        bottom,
                    },
                    ..it
                });
            }
        }
        *items = out;
    }

    /// The whole-surface clip rect (static text never needs tighter clipping).
    fn full_bounds(&self) -> TextBounds {
        TextBounds {
            left: 0,
            top: 0,
            right: self.config.width as i32,
            bottom: self.config.height as i32,
        }
    }

    /// The pixel clip rect of a window's text area (`rows` cells tall from `oy`),
    /// so a sliding line clips at the window edge instead of bleeding over a
    /// neighbour or the status row.
    fn text_bounds(&self, ox: u16, oy: u16, wcols: u16, rows: u16) -> TextBounds {
        TextBounds {
            left: (ox as f32 * self.cell_w) as i32,
            top: (oy as f32 * self.cell_h) as i32,
            right: ((ox + wcols) as f32 * self.cell_w) as i32,
            bottom: ((oy + rows) as f32 * self.cell_h) as i32,
        }
    }

    /// Queue a single-color string at `pos`, clipped to `bounds`.
    fn push_plain(
        &mut self,
        items: &mut Vec<TextItem>,
        text: &str,
        pos: (f32, f32),
        fg: u32,
        bounds: TextBounds,
    ) {
        if text.is_empty() {
            return;
        }
        self.push_text(items, &[Seg::plain(text.to_string(), fg)], pos, fg, bounds);
    }

    /// Paint one gutter cell for buffer line `n` at `pos`: the number formatted per
    /// `number`/`relativenumber` (absolute, distance-from-cursor, or the hybrid on
    /// the cursor line), in `CursorLineNr` on the cursor line and `LineNr`
    /// elsewhere. Mirrors the TUI's `render_gutter`/`gutter_cell`.
    fn push_gutter(
        &mut self,
        items: &mut Vec<TextItem>,
        lines: (usize, usize),
        win: &WindowView,
        view: &View,
        pos: (f32, f32),
        bounds: TextBounds,
    ) {
        let (n, current_line) = lines;
        let text = gutter_cell(
            n,
            current_line,
            win.number,
            win.relativenumber,
            win.number_width as usize,
        );
        // Honor any per-window `winhighlight` override of the gutter groups.
        let color = if n == current_line {
            style_fg(&win.cursor_line_nr(view)).unwrap_or(DEFAULT_FG)
        } else {
            style_fg(&win.line_nr(view)).unwrap_or(DEFAULT_LINE_NR)
        };
        self.push_plain(items, &text, pos, color, bounds);
    }

    /// Queue `segments` (each its own color) at `pos`, clipped to `bounds`,
    /// ensuring the shaped buffer is in the cache. A no-op for an all-empty line,
    /// so blank rows cost nothing.
    fn push_text(
        &mut self,
        items: &mut Vec<TextItem>,
        segments: &[Seg],
        pos: (f32, f32),
        default_fg: u32,
        bounds: TextBounds,
    ) {
        if segments.iter().all(|s| s.text.is_empty()) {
            return;
        }
        let (key, snapped) = self.ensure(segments, default_fg);
        let color = srgb_to_color(default_fg);
        // The common case: every glyph snapped to the cell grid — draw the whole line
        // as one buffer at the run origin, without re-reading the cache entry.
        if snapped {
            items.push(TextItem {
                key,
                x: pos.0,
                y: pos.1,
                color,
                bounds,
                scale: 1.0,
            });
            return;
        }

        // Otherwise the line has off-grid clusters (emoji or monochrome symbols). Mask
        // each to spaces so the rest of the line shapes on-grid (surrounding text stays
        // aligned), then place each cluster as its own scaled item over the reserved gap.
        let nonsnapped = self
            .cache
            .get(&key)
            .map(|e| e.nonsnapped.clone())
            .unwrap_or_default();
        let full: String = segments.iter().map(|s| s.text.as_str()).collect();
        let masked = mask_segments(segments, &nonsnapped);
        let (mkey, _) = self.ensure(&masked, default_fg);
        items.push(TextItem {
            key: mkey,
            x: pos.0,
            y: pos.1,
            color,
            bounds,
            scale: 1.0,
        });

        let (cell_w, cell_h, scale) = (self.cell_w, self.cell_h, self.emoji_scale);
        // Scaling up anchors the glyph at the cell top, so it grows downward and sits
        // low. Lift it by half the height growth — `(scale − 1) / 2` of a cell — to
        // re-center it vertically in the line.
        let y = pos.1 - (scale - 1.0) / 2.0 * cell_h;
        for &(start, end) in &nonsnapped {
            let cluster = &full[start..end];
            let col = full[..start].width() as f32; // cells before the cluster
                                                    // Carry the cluster's own segment style — its colour (so a symbol/kanji in
                                                    // a comment is tinted like the comment), and bold/italic. A color-emoji
                                                    // glyph ignores the fg, but a monochrome icon or CJK glyph needs it.
            let (cfg, bold, italic) = seg_style_at(segments, start)
                .map_or((default_fg, false, false), |s| (s.fg, s.bold, s.italic));
            let (ekey, _) = self.ensure(
                &[Seg {
                    text: cluster.to_string(),
                    fg: cfg,
                    bg: None,
                    bold,
                    italic,
                }],
                cfg,
            );
            // A fallback font shapes a Nerd glyph inside a box wider than its reserved
            // cell, with the *ink* parked at one side of that box: a right-pointing ``
            // hugs the box's left, a left-pointing `` hugs its right (visible in Font
            // Book). Left-anchoring the box at the cell edge then lands a right-hugging
            // glyph a cell to the right — the misaligned right-hand separators. Measure
            // the rasterised ink box: a glyph whose ink hugs the *right* of its advance
            // is right-aligned to the reserved cell, preserving the seamless powerline
            // join; a left- or centre-biased glyph keeps the natural left anchor. The
            // size (`emoji_scale`) is untouched either way.
            let span = cluster.width().max(1) as f32;
            let cell_left = pos.0 + col * cell_w;
            let phys = self
                .cache
                .get(&ekey)
                .and_then(|e| e.buffer.layout_runs().next())
                .and_then(|r| r.glyphs.first())
                .map(|g| (g.w, g.physical((0.0, 0.0), 1.0)));
            let ink = phys.and_then(|(adv, p)| {
                self.swash_cache
                    .get_image(&mut self.font_system, p.cache_key)
                    .as_ref()
                    .filter(|img| img.placement.width > 0)
                    .map(|img| (adv, img.placement.left as f32, img.placement.width as f32))
            });
            // `left_gap`/`right_gap`: padding between the ink and each edge of the glyph's
            // advance box. The smaller gap is the side the ink hugs.
            let x = match ink {
                Some((adv, ink_left, ink_w)) if (adv - (ink_left + ink_w)) < ink_left => {
                    // Ink hugs the right of its box: pin the ink's right edge to the
                    // cell's right edge.
                    cell_left + span * cell_w - scale * (ink_left + ink_w)
                }
                _ => cell_left,
            };
            // A color-emoji font renders smaller than its reserved cells, so draw the
            // glyph at the configured `emoji_scale` (`--emoji-scale`) at its column.
            items.push(TextItem {
                key: ekey,
                x,
                y,
                color: srgb_to_color(cfg),
                bounds,
                scale,
            });
        }
    }

    /// Ensure a shaped buffer for `segments` is cached, returning its key and whether
    /// every cluster snapped to the cell grid (the common case — no off-grid emoji /
    /// symbols, so the caller can draw the line as one buffer without re-reading the
    /// entry). A hit just refreshes the entry's frame stamp — the whole point: no
    /// reshaping for a line whose content hasn't changed. The emoji-cluster list is
    /// computed once here, with the shape.
    fn ensure(&mut self, segments: &[Seg], default_fg: u32) -> (u64, bool) {
        let key = line_key(segments, default_fg);
        if let Some(e) = self.cache.get_mut(&key) {
            e.used = self.gen;
            return (key, e.nonsnapped.is_empty());
        }
        let buffer = self.shape_segments(segments);
        let nonsnapped = nonsnapped_clusters(&buffer, self.cell_w);
        let snapped = nonsnapped.is_empty();
        self.cache.insert(
            key,
            CacheEntry {
                buffer,
                used: self.gen,
                nonsnapped,
            },
        );
        (key, snapped)
    }

    /// Shape `segments` into a fresh glyphon buffer (the expensive op the cache
    /// exists to avoid repeating). Each run carries its own color, a `bold` weight
    /// (cosmic-text picks the closest weight, synthesizing if needed), and a *synthetic*
    /// `italic` skew — see the `FAKE_ITALIC` note below for why italic is not a real
    /// face here.
    fn shape_segments(&mut self, segments: &[Seg]) -> Buffer {
        // Family borrows `fonts`; the buffer borrows `font_system` — disjoint fields,
        // so the two borrows coexist. (The fallback chain lives in `font_system`'s
        // `UserFallback`, so only the primary family is named here.)
        let family = self
            .fonts
            .first()
            .map(|s| Family::Name(s))
            .unwrap_or(Family::Monospace);
        let mut buf = Buffer::new(
            &mut self.font_system,
            Metrics::new(self.font_size, self.line_height),
        );
        let default = Attrs::new().family(family);
        let rich = segments.iter().map(|s| {
            let mut attrs = default.clone().color(srgb_to_color(s.fg));
            if s.bold {
                attrs = attrs.weight(glyphon::Weight::BOLD);
            }
            if s.italic {
                // Synthetic (skewed) italic, not a real italic *face*. Requesting
                // `Style::Italic` makes cosmic-text's matcher demand an italic face for
                // every glyph (`Attrs::matches` gates on exact style) — so a CJK/symbol
                // glyph in an italic comment, which has no italic fallback font, falls
                // through the entire font set (slow), renders as tofu, and skips
                // cell-snapping (dragging the rest of the line off-grid). `FAKE_ITALIC`
                // keeps the regular face — found fast, and snapped by `monospace_width`
                // since its advance is unchanged — and skews it at raster instead.
                attrs = attrs.cache_key_flags(CacheKeyFlags::FAKE_ITALIC);
            }
            (s.text.as_str(), attrs)
        });
        buf.set_rich_text(
            &mut self.font_system,
            rich,
            &default,
            Shaping::Advanced,
            None,
        );
        // Snap every glyph's advance to a whole number of cells: cosmic-text scales each
        // glyph so its advance is a multiple of this width, so a wide CJK/emoji glyph
        // occupies two cells (centered in its 2-em box by the font) and the text after it
        // stays on the grid — the GUI's monospace-cell contract, which plain `Advanced`
        // shaping (font-native advances) breaks. This relayouts + reshapes.
        //
        // CRUCIAL: this is a silent no-op unless cosmic-text is built with the
        // `monospace_fallback` feature (it gates `Font::monospace_em_width`, which the
        // scaling reads). nxvim-gui pulls cosmic-text in directly only to enable it — see
        // the dep note in the workspace `Cargo.toml`. Don't drop that dep.
        let cell_w = self.cell_w;
        buf.set_monospace_width(&mut self.font_system, Some(cell_w));
        buf.shape_until_scroll(&mut self.font_system, false);
        buf
    }
}

/// The built-in diagnostic underline color for a severity (`1`=error … `4`=hint),
/// used when no colorscheme defines `DiagnosticUnderline*`. Mirrors the TUI's
/// `severity_color`.
fn severity_color(severity: u8) -> u32 {
    match severity {
        2 => DIAG_WARN,
        3 => DIAG_INFO,
        4 => DIAG_HINT,
        _ => DIAG_ERROR, // error (and any unexpected code)
    }
}

/// Resolve a diagnostic's paint color: the highlight group `id` (when the
/// colorscheme defined it) else the built-in [`severity_color`] for `severity`.
/// `prefer_sp` picks the group's `sp` (special) color before its `fg` — underlines
/// use the special color, signs and virtual text use the foreground.
fn diag_color(styles: &[Style], id: Option<usize>, severity: u8, prefer_sp: bool) -> u32 {
    id.and_then(|id| styles.get(id))
        .and_then(|st| if prefer_sp { st.sp.or(st.fg) } else { st.fg })
        .unwrap_or_else(|| severity_color(severity))
}

/// Subtract the rect `hole` from rect `a` (both `(left, top, right, bottom)` in
/// pixels), returning the ≤4 disjoint pieces of `a` not covered by `hole` — the
/// whole of `a` when they don't overlap, nothing when `hole` fully covers `a`.
/// Used to clip a lower float's text so a higher float drawn over it reads as
/// opaque (see [`Renderer::occlude_overlay_text`]). The pieces are emitted as
/// top / bottom strips (full width) plus left / right strips (between them), so
/// they tile `a \ hole` without gaps or overlap.
pub fn rect_subtract(
    a: (i32, i32, i32, i32),
    hole: (i32, i32, i32, i32),
) -> Vec<(i32, i32, i32, i32)> {
    let (al, at, ar, ab) = a;
    // Intersection of `a` and `hole`.
    let (il, it, ir, ib) = (
        al.max(hole.0),
        at.max(hole.1),
        ar.min(hole.2),
        ab.min(hole.3),
    );
    if il >= ir || it >= ib {
        return vec![a]; // disjoint (or zero-area overlap) → `a` is untouched
    }
    let mut out = Vec::with_capacity(4);
    if at < it {
        out.push((al, at, ar, it)); // strip above the hole
    }
    if ib < ab {
        out.push((al, ib, ar, ab)); // strip below the hole
    }
    if al < il {
        out.push((al, it, il, ib)); // strip left of the hole (hole's row band)
    }
    if ir < ar {
        out.push((ir, it, ar, ib)); // strip right of the hole (hole's row band)
    }
    out
}

/// Lighten a packed `0xRRGGBB` color by adding `d` to each channel (saturating) —
/// how the overlay surfaces (floats, popups, their borders) lift a region off the
/// editor background, which truecolor has no reverse-video shortcut for.
fn lighten(c: u32, d: u8) -> u32 {
    let f = |b: u8| b.saturating_add(d) as u32;
    (f((c >> 16) as u8) << 16) | (f((c >> 8) as u8) << 8) | f(c as u8)
}

/// The opaque background for an overlay surface (a float / popup box): the
/// colorscheme's `NormalFloat`, else the editor background [`lighten`]ed by
/// `lighten_amt` so the box lifts off the text behind it.
fn float_bg(view: &View, lighten_amt: u8) -> u32 {
    style_bg(&view.normal_float)
        .unwrap_or_else(|| lighten(style_bg(&view.normal).unwrap_or(DEFAULT_BG), lighten_amt))
}

/// Inset a `(x, y, w, h)` cell rect by a window's `'padding'` — a per-side blank
/// margin — clamped so at least a 1×1 cell survives. The window content paints
/// into the returned rect; the margin shows whatever is behind it (the editor
/// background, or a float's box fill).
fn pad_rect(rect: (u16, u16, u16, u16), pad: nxvim_view::Padding) -> (u16, u16, u16, u16) {
    let (x, y, w, h) = rect;
    let left = pad.left.min(w.saturating_sub(1));
    let top = pad.top.min(h.saturating_sub(1));
    (
        x + left,
        y + top,
        w.saturating_sub(left + pad.right).max(1),
        h.saturating_sub(top + pad.bottom).max(1),
    )
}

/// First visible completion-item index for a popup `rows` tall with `selected`
/// highlighted: scroll the list to keep the selection in view, else start at the
/// top. Mirrors the TUI's `pmenu_start`.
fn pmenu_start(selected: Option<usize>, rows: usize) -> usize {
    match selected {
        Some(s) if rows > 0 && s >= rows => s + 1 - rows,
        _ => 0,
    }
}

/// One completion row padded to `width` cells: the `label` left-aligned and the
/// Shorten `s` to at most `width` chars, keeping its head and tail and dropping the
/// middle behind a single `…` when it won't fit — so a too-long preview path shows
/// both its root and its filename instead of just the start. Mirrors the TUI's
/// `elide_middle`.
fn elide_middle(s: &str, width: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= width {
        return s.to_string();
    }
    if width <= 1 {
        return chars.iter().take(width).collect();
    }
    let keep = width - 1; // one column for the `…`
    let front = keep / 2; // favour the tail (filename) with the larger half
    let back = keep - front;
    let head: String = chars[..front].iter().collect();
    let tail: String = chars[chars.len() - back..].iter().collect();
    format!("{head}…{tail}")
}

/// `detail` (a type/source hint) right-aligned when it fits after a one-cell gap;
/// a too-long label is truncated. Mirrors the TUI's `pmenu_row`.
fn pmenu_row(label: &str, detail: &str, width: usize) -> String {
    let label: String = label.chars().take(width).collect();
    let label_w = label.chars().count();
    let detail_w = detail.chars().count();
    if !detail.is_empty() && label_w + 1 + detail_w <= width {
        let pad = width - label_w - detail_w;
        format!("{label}{}{detail}", " ".repeat(pad))
    } else {
        format!("{label:<width$}")
    }
}

/// Truncate a picker row `label` to `width` columns while keeping the file name (the
/// path tail) visible: when a path overflows the row, drop whole leading directory
/// components behind a single `…` so the file name survives, instead of the plain
/// head-cut that would hide the very thing you scan for. `spans` (matched-char char
/// ranges into `label`) are remapped onto the returned string so highlights stay
/// aligned. A row that fits — or a non-path row (no `/`) — is returned unchanged.
/// Mirrors the TUI's `elide_keep_tail`.
fn elide_keep_tail(label: &str, spans: &[(u16, u16)], width: usize) -> (String, Vec<(u16, u16)>) {
    let chars: Vec<char> = label.chars().collect();
    let n = chars.len();
    if n <= width || width == 0 || !label.contains('/') {
        return (label.to_string(), spans.to_vec());
    }
    // Reserve one column for the leading `…`; keep at most the last `width - 1` chars.
    let drop = n - (width - 1);
    // Prefer a clean cut just after a path separator — the smallest `/`-boundary at
    // or past `drop` keeps the most directory context that still fits. None ⇒ raw cut.
    let cut = (drop..n).find(|&i| chars[i - 1] == '/').unwrap_or(drop);
    let mut out = String::with_capacity(width);
    out.push('…');
    out.extend(&chars[cut..]);
    // Remap spans: original index `i` (≥ cut) renders at display index `i - cut + 1`
    // (the `…` occupies index 0). A span wholly inside the dropped prefix vanishes.
    let shift = cut as i64 - 1;
    let remapped = spans
        .iter()
        .filter_map(|&(s, e)| {
            let ns = (s as i64).max(cut as i64) - shift;
            let ne = (e as i64).min(n as i64) - shift;
            (ns < ne).then_some((ns as u16, ne as u16))
        })
        .collect();
    (out, remapped)
}

/// One `width`-cell gutter cell for buffer line `n` (the cursor sits on
/// `current_line`): absolute numbers (`number`), distance-from-cursor
/// (`relativenumber`), or the hybrid — absolute on the cursor line, relative
/// elsewhere. Numbers are right-aligned with a trailing space, except the hybrid
/// cursor line whose absolute number is left-aligned. Mirrors the TUI's
/// `gutter_cell`.
fn gutter_cell(
    n: usize,
    current_line: usize,
    number: bool,
    relativenumber: bool,
    width: usize,
) -> String {
    let is_current = n == current_line;
    if number && relativenumber && is_current {
        // Hybrid cursor line: absolute number, left-aligned.
        format!("{n:<width$}")
    } else {
        let value = if !relativenumber {
            n // number-only: absolute on every line
        } else if is_current {
            0 // relativenumber-only cursor line shows 0
        } else {
            n.abs_diff(current_line)
        };
        let field = width.saturating_sub(1); // reserve the trailing space
        format!("{value:>field$} ")
    }
}

/// The command-line caret's screen cell (cell 0 = the line's first cell): the
/// leading prompt — the single-cell `:`/`/`/`?` prefix, or the multi-char
/// `vim.ui.input` label — followed by the text before the caret. `cursor_chars`
/// (`View::cmdline_cursor`) is a **char offset** on the wire (nxvim-core's
/// `cmdline_cursor()`), and the row is painted as one shaped run in which a wide
/// CJK/emoji char occupies two cells, so both the prompt and the pre-caret text
/// must be measured by display width — counting chars lands the caret one cell
/// short per wide char, out from under the glyph it edits (the server measures
/// the prompt with `display_width` too). Pure, so it's unit-tested in
/// `tests/caret.rs`.
pub fn cmdline_caret_col(prompt: &str, line: &str, cursor_chars: usize) -> u16 {
    let prompt_cells = if prompt.is_empty() {
        1 // `:` / `/` / `?` — a single-cell prefix
    } else {
        prompt.width() as u16
    };
    prompt_cells + cells_before(line, cursor_chars)
}

/// The picker prompt caret's cell offset within the list column: the `"> "`
/// prefix (two cells) plus the query text before the caret. `cursor_chars`
/// (`MenuData::query_cursor`) is a **char offset** on the wire, measured onto
/// the shaped run by display width — the same wide-char rationale as
/// [`cmdline_caret_col`]. Pure, so it's unit-tested in `tests/caret.rs`.
pub fn query_caret_col(query: &str, cursor_chars: usize) -> u16 {
    2 + cells_before(query, cursor_chars)
}

/// The display-width cells of the first `chars` chars of `s` — how a char-offset
/// wire field maps onto the shaped run's cell grid, where a wide CJK/emoji char
/// takes two cells and a zero-width combining mark takes none. An offset past the
/// end clamps to the string's full width (`take` saturates).
fn cells_before(s: &str, chars: usize) -> u16 {
    s.chars()
        .take(chars)
        .map(|c| c.width().unwrap_or(0))
        .sum::<usize>() as u16
}

/// Map a buffer screen-column to its absolute screen cell in a window whose text
/// area starts at `text_x0` and is horizontally scrolled by `leftcol` columns: the
/// column slides left by `leftcol`, clamping at `text_x0` for anything scrolled off
/// the left edge. The cursor, the selection/search quads, and the underline rules
/// all key off this one mapping, so the text origin ([`text_run_origin`]) must too —
/// otherwise scrolled glyphs drift out from under their own overlays.
pub fn col_to_screen(text_x0: u16, col: u16, leftcol: u16) -> u16 {
    text_x0 + col.saturating_sub(leftcol)
}

/// The absolute screen cell where a window's text run begins under a `leftcol`
/// horizontal scroll. [`row_segments`]/[`splice_inlay`] already drop the `leftcol`
/// columns scrolled off the left, so the surviving run starts at the first *visible*
/// buffer column (`== leftcol`), which maps to `text_x0` via [`col_to_screen`].
/// Subtracting `leftcol` a second time here (as the origin once did) double-counts
/// the scroll and shoves the glyphs `leftcol` cells left — over the number gutter
/// and out from under the cursor. So this is `text_x0`, independent of `leftcol`.
pub fn text_run_origin(text_x0: u16, leftcol: u16) -> u16 {
    col_to_screen(text_x0, leftcol, leftcol)
}

/// Split a tab-expanded row into `(text, color)` segments from its highlight
/// spans; uncovered runs take the default `fg`. Columns are screen columns, so we
/// slice by char index (== screen column for ASCII/tab text; wide-char fidelity
/// is deferred). Pure, so it can run before the cache lookup keys off the result.
pub fn row_segments(
    display: &str,
    hl: &[nxvim_view::HlSpan],
    styles: &[Style],
    fg: u32,
    normal_bg: u32,
    leftcol: u16,
) -> Vec<Seg> {
    let chars: Vec<char> = display.chars().collect();
    let mut segments: Vec<Seg> = Vec::new();
    let mut col = leftcol as usize;
    let n = chars.len();
    // Sort spans by start so the walk is monotonic. `HlSpan` is the tuple
    // `(start, end, group, style_id)`.
    let mut spans: Vec<&nxvim_view::HlSpan> = hl.iter().collect();
    spans.sort_by_key(|s| s.0);
    for s in spans {
        let start = (s.0 as usize).max(col);
        let end = (s.1 as usize).min(n);
        if end <= start {
            continue;
        }
        if start > col {
            segments.push(Seg::plain(chars[col..start].iter().collect(), fg));
        }
        let text: String = chars[start..end].iter().collect();
        match s.3.and_then(|id| styles.get(id)) {
            Some(st) => {
                // Reverse swaps fg/bg: the glyph takes the style's background (or the
                // editor's `Normal` bg) and a foreground-colored quad behind it is
                // painted by `push_reverse_fills`, so the run reads inverted — its own
                // `bg` stays unset (the reverse fill is its background). A non-reverse
                // run keeps its fg and carries its group's `bg` (when set) as a quad
                // painted behind the glyph by `push_seg_backgrounds` — e.g. a diff line
                // tint, or any colorscheme group with a background.
                let (color, bg) = if st.reverse {
                    (st.bg.unwrap_or(normal_bg), None)
                } else {
                    (st.fg.unwrap_or(fg), st.bg)
                };
                segments.push(Seg {
                    text,
                    fg: color,
                    bg,
                    bold: st.bold,
                    italic: st.italic,
                });
            }
            // No colorscheme resolved this span: fall back to a built-in color for
            // its capture group, so a buffer still highlights with no theme loaded.
            None => {
                let (color, italic) = group_fallback(&s.2, fg);
                segments.push(Seg {
                    text,
                    fg: color,
                    bg: None,
                    bold: false,
                    italic,
                });
            }
        }
        col = end;
    }
    if col < n {
        segments.push(Seg::plain(chars[col..n].iter().collect(), fg));
    }
    segments
}

/// Built-in syntax color for a treesitter capture `group` when no colorscheme
/// resolved it (`style_id` is `None`) — the GUI's truecolor analogue of the TUI's
/// `group_style`, so a buffer highlights even with no colorscheme loaded. Keys off
/// the group's major component (before the first `.`), in One Dark hues that match
/// the `DIAG_*` fallbacks already used here. Returns the foreground and whether the
/// run is italic (comments); an unmapped group keeps the default `fg`.
pub fn group_fallback(group: &str, fg: u32) -> (u32, bool) {
    let major = group.split('.').next().unwrap_or(group);
    let color = match major {
        "keyword" | "conditional" | "repeat" | "include" | "exception" | "keyword_operator" => {
            0xc6_78_dd
        } // purple
        "function" | "method" => 0x61_af_ef, // blue
        "constructor" | "type" | "namespace" | "module" => 0xe5_c0_7b, // yellow
        "string" | "character" => 0x98_c3_79, // green
        "number" | "boolean" | "float" | "constant" => 0x56_b6_c2, // cyan
        "attribute" | "label" | "property" | "field" => 0x56_b6_c2, // cyan
        "comment" => return (0x5c_63_70, true), // grey, italic
        "tag" => 0xe0_6c_75,                 // red
        "operator" | "punctuation" => 0xab_b2_bf, // grey
        _ => fg,
    };
    (color, false)
}

/// The combined cell width of the inlay hints on a row that fall at or before
/// (`inclusive`) — or strictly before (`!inclusive`) — screen column `col`, with
/// hints scrolled off the left (`hcol < leftcol`) excluded. This is how far the
/// inline splice pushes a glyph/overlay at `col` to the right: a left edge / the
/// cursor uses `inclusive` (a hint *at* the column sits before it); a right edge
/// uses `!inclusive` (a hint *at* the column is past it). Hint width is char count,
/// matching the renderer's ASCII-column convention. Mirrors the TUI's
/// `inlay_cursor_shift` / the per-glyph shift its inline splice accumulates.
pub fn inlay_shift(inlay: &[InlayHint], leftcol: u16, col: u16, inclusive: bool) -> u16 {
    inlay
        .iter()
        .filter(|(hcol, _, _)| {
            *hcol >= leftcol && (if inclusive { *hcol <= col } else { *hcol < col })
        })
        .map(|(_, text, _)| text.chars().count() as u16)
        .sum()
}

/// Splice a row's inlay hints into its colored `base` segments at their anchor
/// columns, pushing the following text right — the GUI analogue of the TUI's inline
/// splice. `base` covers screen columns `[leftcol, n)` contiguously (every gap is a
/// plain run), so we walk it tracking the current column and, before the glyph at a
/// hint's column, flush the pending run and emit the hint as its own segment styled
/// by its resolved `LspInlayHint` color (or [`DEFAULT_INLAY`] when undefined). A
/// hint scrolled off the left is dropped; hints at or past end-of-text are appended.
/// The shaper then lays the segments out contiguously, so the hint cells sit inline
/// and shift the real glyphs — the column-keyed overlays shift to match via
/// [`inlay_shift`]. With no hints `base` is returned untouched.
pub fn splice_inlay(
    base: Vec<Seg>,
    inlay: &[InlayHint],
    leftcol: u16,
    styles: &[Style],
) -> Vec<Seg> {
    if inlay.is_empty() {
        return base;
    }
    let mut out: Vec<Seg> = Vec::with_capacity(base.len() + inlay.len() * 2);
    let mut col = leftcol as usize;
    let mut hi = 0usize;
    for seg in base {
        let seg_chars: Vec<char> = seg.text.chars().collect();
        let mut start = 0usize; // first char of the current pending run within this seg
        for k in 0..seg_chars.len() {
            let c = col + k;
            if hi < inlay.len() && (inlay[hi].0 as usize) <= c {
                if k > start {
                    out.push(Seg {
                        text: seg_chars[start..k].iter().collect(),
                        fg: seg.fg,
                        bg: seg.bg,
                        bold: seg.bold,
                        italic: seg.italic,
                    });
                }
                push_hint_segs(&mut out, inlay, &mut hi, c, leftcol, styles);
                start = k;
            }
        }
        if start < seg_chars.len() {
            out.push(Seg {
                text: seg_chars[start..].iter().collect(),
                fg: seg.fg,
                bg: seg.bg,
                bold: seg.bold,
                italic: seg.italic,
            });
        }
        col += seg_chars.len();
    }
    // Hints anchored at or past end-of-text (e.g. an end-of-line type annotation).
    push_hint_segs(&mut out, inlay, &mut hi, usize::MAX, leftcol, styles);
    out
}

/// Emit every not-yet-emitted hint whose column is `<= upto` as its own [`Seg`],
/// advancing `hi`. A hint scrolled off the left (`hcol < leftcol`) is consumed but
/// not painted. The hint color is its resolved `LspInlayHint` style's foreground,
/// or [`DEFAULT_INLAY`] when the colorscheme leaves the group undefined.
fn push_hint_segs(
    out: &mut Vec<Seg>,
    inlay: &[InlayHint],
    hi: &mut usize,
    upto: usize,
    leftcol: u16,
    styles: &[Style],
) {
    while *hi < inlay.len() && (inlay[*hi].0 as usize) <= upto {
        let (hcol, text, id) = &inlay[*hi];
        *hi += 1;
        if *hcol < leftcol {
            continue; // scrolled off the left edge
        }
        let color = id
            .and_then(|i| styles.get(i))
            .and_then(|s| s.fg)
            .unwrap_or(DEFAULT_INLAY);
        out.push(Seg::plain(text.clone(), color));
    }
}

/// Placement `pos` tags on the `virt_text` wire (mirror the server's `VirtTextPos`
/// and the TUI's `VIRT_POS_*`): 0=eol, 1=inline, 2=overlay, 3=right_align,
/// 4=win_col.
const VIRT_POS_EOL: u8 = 0;
const VIRT_POS_INLINE: u8 = 1;
const VIRT_POS_OVERLAY: u8 = 2;
const VIRT_POS_RIGHT_ALIGN: u8 = 3;
const VIRT_POS_WIN_COL: u8 = 4;

/// Resolve a virtual-text chunk to a GUI [`Seg`]: the colorscheme palette entry the
/// server interned for its `hl_group` (foreground + bold/italic), else the window's
/// normal `fg`. The GUI paints glyph **foregrounds** (backgrounds are quads), so a
/// chunk's background and the `hl_mode` background merge aren't modelled — only the
/// foreground (deferred bg fidelity, like wide-char).
fn virt_chunk_seg(text: &str, id: Option<usize>, styles: &[Style], fg: u32) -> Seg {
    match id.and_then(|i| styles.get(i)) {
        Some(st) => {
            // `reverse` swaps fg/bg (matching the base text path); otherwise the
            // group's own fg/bg. A `bg` is painted as a quad behind the chunk.
            let (f, b) = if st.reverse {
                (st.bg, st.fg)
            } else {
                (st.fg, st.bg)
            };
            Seg {
                text: text.to_string(),
                fg: f.unwrap_or(fg),
                bg: b,
                bold: st.bold,
                italic: st.italic,
            }
        }
        None => Seg::plain(text.to_string(), fg),
    }
}

/// The foreground for an overlay/win_col chunk over a cell whose glyph foreground is
/// `under_fg`, per `hl_mode`: `replace` (0) and `combine` (1) keep the chunk's own
/// fg (it is the set attribute); `blend` (2) averages the two channel-wise — a
/// coarse terminal-cell analogue of neovim's alpha blend (exact pixel blending isn't
/// expressible per glyph). Mirrors the TUI's [`apply_hl_mode`] at fg fidelity.
fn virt_overlay_fg(chunk_fg: u32, under_fg: u32, mode: u8) -> u32 {
    if mode == 2 {
        let mix = |sh: u32| {
            let a = (chunk_fg >> sh) & 0xff;
            let b = (under_fg >> sh) & 0xff;
            (a + b) / 2
        };
        (mix(16) << 16) | (mix(8) << 8) | mix(0)
    } else {
        chunk_fg
    }
}

/// The combined cell width of the inline `virt_text` placements on a row at or
/// before screen column `col` (with placements scrolled off the left excluded) —
/// how far the inline splice pushes a glyph/cursor at `col` to the right. The
/// `virt_text` analogue of [`inlay_shift`]; the cursor adds both.
fn virt_inline_shift(vtext: &[VirtPlacement], leftcol: u16, col: u16) -> u16 {
    vtext
        .iter()
        .filter(|p| p.pos == VIRT_POS_INLINE && p.col >= leftcol && p.col <= col)
        .flat_map(|p| p.chunks.iter())
        .map(|(t, _)| t.chars().count() as u16)
        .sum()
}

/// Splice a column-anchored insertion list into a row's colored `base` segments,
/// pushing later text right — the generalization of [`splice_inlay`] that carries
/// both inlay hints and inline `virt_text`. Each insertion is already-styled
/// segments at an **original screen column**; `base` covers `[leftcol, n)`
/// contiguously, so we walk it and, before the glyph at an insertion's column,
/// flush the pending run and emit the insertion. Insertions must be sorted ascending
/// by column (ties keep list order — the caller orders inlay before inline virt at a
/// shared column, matching the TUI's walk); those at or past end-of-text are appended.
fn splice_insertions(base: Vec<Seg>, insertions: &[(u16, Vec<Seg>)], leftcol: u16) -> Vec<Seg> {
    if insertions.is_empty() {
        return base;
    }
    let mut out: Vec<Seg> = Vec::with_capacity(base.len() + insertions.len() * 2);
    let mut col = leftcol as usize;
    let mut ii = 0usize;
    let emit_at = |out: &mut Vec<Seg>, ii: &mut usize, c: usize| {
        while *ii < insertions.len() && (insertions[*ii].0 as usize) <= c {
            out.extend(insertions[*ii].1.iter().cloned());
            *ii += 1;
        }
    };
    for seg in base {
        let seg_chars: Vec<char> = seg.text.chars().collect();
        let mut start = 0usize;
        for k in 0..seg_chars.len() {
            let c = col + k;
            if ii < insertions.len() && (insertions[ii].0 as usize) <= c {
                if k > start {
                    out.push(Seg {
                        text: seg_chars[start..k].iter().collect(),
                        fg: seg.fg,
                        bg: seg.bg,
                        bold: seg.bold,
                        italic: seg.italic,
                    });
                }
                emit_at(&mut out, &mut ii, c);
                start = k;
            }
        }
        if start < seg_chars.len() {
            out.push(Seg {
                text: seg_chars[start..].iter().collect(),
                fg: seg.fg,
                bg: seg.bg,
                bold: seg.bold,
                italic: seg.italic,
            });
        }
        col += seg_chars.len();
    }
    // Insertions anchored at or past end-of-text.
    while ii < insertions.len() {
        out.extend(insertions[ii].1.iter().cloned());
        ii += 1;
    }
    out
}

/// Build a row's final segments by overwriting the cells covered by `overlay` /
/// `win_col` `virt_text` (no shift — they replace the glyphs they cover, honoring
/// `hl_mode` at fg fidelity) and splicing in the inline insertions — LSP inlay hints
/// **and** inline `virt_text` — which push later glyphs right. The GUI analogue of
/// the TUI's single-walk `highlight_line`. Used only when a row actually carries
/// `virt_text`; the common (no-virt) row keeps the cheaper [`splice_inlay`] path.
fn apply_row_virt(
    base: Vec<Seg>,
    inlay: &[InlayHint],
    vtext: &[VirtPlacement],
    leftcol: u16,
    styles: &[Style],
    fg: u32,
) -> Vec<Seg> {
    // Expand `base` to a per-cell grid (its first cell is at absolute column
    // `leftcol`), so overlay/win_col can overwrite individual columns. Each cell
    // carries `(char, fg, bg, bold, italic)` so an overlay's background rides through.
    let mut cells: Vec<(char, u32, Option<u32>, bool, bool)> = Vec::new();
    for seg in &base {
        for ch in seg.text.chars() {
            cells.push((ch, seg.fg, seg.bg, seg.bold, seg.italic));
        }
    }
    for p in vtext
        .iter()
        .filter(|p| p.pos == VIRT_POS_OVERLAY || p.pos == VIRT_POS_WIN_COL)
    {
        let mut abs = p.col as usize; // absolute screen column the overlay starts on
        for (text, id) in &p.chunks {
            let seg = virt_chunk_seg(text, *id, styles, fg);
            for ch in text.chars() {
                if abs < leftcol as usize {
                    abs += 1; // scrolled off the left edge
                    continue;
                }
                let k = abs - leftcol as usize;
                let under_fg = cells.get(k).map(|c| c.1).unwrap_or(fg);
                let ofg = virt_overlay_fg(seg.fg, under_fg, p.hl_mode);
                let cell = (ch, ofg, seg.bg, seg.bold, seg.italic);
                if k < cells.len() {
                    cells[k] = cell;
                } else {
                    // Past end-of-text (a fixed-column guide on a short line): pad with
                    // blanks up to the column, then place the glyph.
                    while cells.len() < k {
                        cells.push((' ', fg, None, false, false));
                    }
                    cells.push(cell);
                }
                abs += 1;
            }
        }
    }
    // Recompress the grid into coalesced runs of identical style.
    let mut segs: Vec<Seg> = Vec::new();
    for (ch, f, bg, b, it) in cells {
        match segs.last_mut() {
            Some(last) if last.fg == f && last.bg == bg && last.bold == b && last.italic == it => {
                last.text.push(ch)
            }
            _ => segs.push(Seg {
                text: ch.to_string(),
                fg: f,
                bg,
                bold: b,
                italic: it,
            }),
        }
    }
    // Inline insertions: inlay hints (their resolved `LspInlayHint` fg, else
    // `DEFAULT_INLAY`) and inline `virt_text` chunks, keyed on original column.
    // A stable sort keeps inlay before inline virt at a shared column (the order the
    // TUI's walk emits them).
    let mut insertions: Vec<(u16, Vec<Seg>)> = Vec::new();
    for (hcol, text, id) in inlay {
        if *hcol < leftcol {
            continue; // scrolled off the left edge
        }
        let color = id
            .and_then(|i| styles.get(i))
            .and_then(|s| s.fg)
            .unwrap_or(DEFAULT_INLAY);
        insertions.push((*hcol, vec![Seg::plain(text.clone(), color)]));
    }
    for p in vtext.iter().filter(|p| p.pos == VIRT_POS_INLINE) {
        if p.col < leftcol {
            continue;
        }
        let chunk_segs: Vec<Seg> = p
            .chunks
            .iter()
            .map(|(t, id)| virt_chunk_seg(t, *id, styles, fg))
            .collect();
        insertions.push((p.col, chunk_segs));
    }
    insertions.sort_by_key(|(c, _)| *c);
    splice_insertions(segs, &insertions, leftcol)
}

/// Content hash for the shaped-buffer cache: the segments' text + colors and the
/// default fg fully determine the shaped output, so identical content (even at a
/// different screen row) shares one buffer.
fn line_key(segments: &[Seg], default_fg: u32) -> u64 {
    let mut h = DefaultHasher::new();
    default_fg.hash(&mut h);
    for s in segments {
        s.text.hash(&mut h);
        s.fg.hash(&mut h);
        s.bold.hash(&mut h);
        s.italic.hash(&mut h);
    }
    h.finish()
}

/// Measure the cell size from the configured font by shaping a single `M`.
fn measure_cell(
    font_system: &mut FontSystem,
    family: Family,
    font_size: f32,
    line_height: f32,
) -> (f32, f32) {
    let mut buf = Buffer::new(font_system, Metrics::new(font_size, line_height));
    buf.set_text(
        font_system,
        "M",
        &Attrs::new().family(family),
        Shaping::Advanced,
    );
    buf.shape_until_scroll(font_system, false);
    let advance = buf
        .layout_runs()
        .next()
        .and_then(|run| run.glyphs.first().map(|g| g.w))
        .unwrap_or(font_size * 0.6);
    (advance.max(1.0), line_height.max(1.0))
}

/// Byte ranges (into the buffer's line text) of glyph clusters whose advance did
/// **not** land on the cell grid — emoji *and* monochrome symbols (Nerd Font icons,
/// powerline separators) from non-monospace fallback fonts that `set_monospace_width`
/// leaves at their native advance. A glyph is on-grid when its advance is ~a whole
/// number of `cell_w`; a (near-)zero-advance glyph (a combining mark / joiner) is
/// skipped. Consecutive off-grid glyphs with touching byte ranges merge into one
/// cluster, so a multi-glyph emoji (a flag, a ligated ZWJ sequence) is one unit.
fn nonsnapped_clusters(buf: &Buffer, cell_w: f32) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for run in buf.layout_runs() {
        for g in run.glyphs.iter() {
            let cells = g.w / cell_w;
            if cells < 0.5 || (cells - cells.round()).abs() <= 0.15 {
                continue; // narrow/zero advance, or already a whole cell-count wide
            }
            let (s, e) = (g.start.min(g.end), g.start.max(g.end));
            match out.last_mut() {
                Some(last) if s <= last.1 => {
                    last.0 = last.0.min(s);
                    last.1 = last.1.max(e);
                }
                _ => out.push((s, e)),
            }
        }
    }
    out
}

/// The segment covering byte `byte` in the concatenated segment text (the run that
/// owns a masked off-grid cluster), so its colour / weight / slant can be carried onto
/// the separately-drawn glyph. `None` only if `byte` is past the end.
fn seg_style_at(segments: &[Seg], byte: usize) -> Option<&Seg> {
    let mut off = 0;
    for s in segments {
        let end = off + s.text.len();
        if byte < end {
            return Some(s);
        }
        off = end;
    }
    None
}

/// Rebuild `segments`, replacing each byte range in `bad` (off-grid clusters, in
/// concatenated-text coordinates) with as many spaces as the cluster's cell width.
/// The spaces hold the cluster's place on the grid so the rest of the line shapes in
/// alignment; the cluster itself is then drawn as a separate item over the gap. Pure,
/// so it's unit-tested in `tests/wide.rs`.
pub fn mask_segments(segments: &[Seg], bad: &[(usize, usize)]) -> Vec<Seg> {
    let mut out = Vec::with_capacity(segments.len());
    let mut off = 0usize; // byte offset of this segment in the concatenation
    for seg in segments {
        let seg_start = off;
        let seg_end = off + seg.text.len();
        off = seg_end;
        // The bad ranges overlapping this segment, clipped to it.
        let mut overlaps: Vec<(usize, usize)> = bad
            .iter()
            .filter(|&&(s, e)| s < seg_end && e > seg_start)
            .map(|&(s, e)| (s.max(seg_start), e.min(seg_end)))
            .collect();
        if overlaps.is_empty() {
            out.push(seg.clone());
            continue;
        }
        overlaps.sort_unstable();
        let mut text = String::with_capacity(seg.text.len());
        let mut cur = seg_start;
        for (s, e) in overlaps {
            text.push_str(&seg.text[cur - seg_start..s - seg_start]);
            let removed = &seg.text[s - seg_start..e - seg_start];
            for _ in 0..removed.width().max(1) {
                text.push(' ');
            }
            cur = e;
        }
        text.push_str(&seg.text[cur - seg_start..]);
        out.push(Seg {
            text,
            fg: seg.fg,
            bg: seg.bg,
            bold: seg.bold,
            italic: seg.italic,
        });
    }
    out
}

/// A sign glyph fitted to exactly `width` cells: truncated if too wide, then
/// right-padded with spaces (so a 1-cell `E` fills the 2-cell column as `E `).
/// Char count stands in for display width here, matching the renderer's
/// ASCII-column simplification elsewhere (wide-char fidelity is deferred).
fn pad_to_width(s: &str, width: usize) -> String {
    let mut out: String = s.chars().take(width).collect();
    let painted = out.chars().count();
    out.push_str(&" ".repeat(width.saturating_sub(painted)));
    out
}

/// Expand `\t` to spaces up to the next `tabstop` multiple, so a char index in
/// the result equals a screen column for ASCII/tab text.
fn expand_tabs(line: &str, tabstop: usize) -> String {
    if !line.contains('\t') {
        return line.to_string(); // hot path: one memcpy, no per-char walk
    }
    let mut out = String::with_capacity(line.len());
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let n = tabstop - (col % tabstop);
            for _ in 0..n {
                out.push(' ');
            }
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
}

// --- color helpers ---------------------------------------------------------

fn style_fg(s: &Option<Style>) -> Option<u32> {
    s.as_ref().and_then(|st| st.fg)
}
fn style_bg(s: &Option<Style>) -> Option<u32> {
    s.as_ref().and_then(|st| st.bg)
}

/// Resolved colors for the built-in tabline cells, from the theme's `TabLine`
/// (inactive cells + title), `TabLineSel` (active cell) and `TabLineFill` (the bar
/// background) groups. Each falls back to the status-line tint / reverse-video when
/// the colorscheme leaves the group undefined, so the tabline stays drawn either way.
#[derive(Clone, Copy)]
struct TablineColors {
    /// The bar background (and the ground for the title / inactive cells).
    fill_bg: u32,
    /// Inactive tab + title foreground.
    inactive_fg: u32,
    /// Inactive tab background, painted as a per-cell quad only when the theme gives
    /// `TabLine` its own bg distinct from the bar fill (`None` ⇒ the fill shows through).
    inactive_bg: Option<u32>,
    /// Active tab foreground.
    active_fg: u32,
    /// Active tab background (always a filled cell).
    active_bg: u32,
}

impl TablineColors {
    fn resolve(view: &View) -> Self {
        let status_bg = style_bg(&view.status_line).unwrap_or(0x1a_1a_1a);
        let status_fg = style_fg(&view.status_line).unwrap_or(DEFAULT_FG);
        Self {
            fill_bg: style_bg(&view.tabline_fill).unwrap_or(status_bg),
            inactive_fg: style_fg(&view.tabline_style).unwrap_or(status_fg),
            inactive_bg: style_bg(&view.tabline_style),
            // Without a `TabLineSel`, the active cell is the reverse-video of the
            // status line (status fg becomes its ground), matching the old look.
            active_fg: style_fg(&view.tabline_sel).unwrap_or(status_bg),
            active_bg: style_bg(&view.tabline_sel).unwrap_or(status_fg),
        }
    }
}

/// 0xRRGGBB → opaque glyphon [`Color`].
pub fn srgb_to_color(c: u32) -> Color {
    Color::rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}
pub fn srgb_to_color_rgba(c: u32, alpha: f32) -> [f32; 4] {
    let lin = srgb_u32_to_linear(c);
    [lin[0], lin[1], lin[2], alpha]
}
pub fn color_to_rgba(c: Color) -> [f32; 4] {
    // glyphon Color is sRGB bytes; our quad pipeline targets an sRGB surface, so
    // convert to linear (the GPU applies the sRGB encode on store). cosmic-text packs
    // `Color.0` as `0xAARRGGBB`, so its little-endian bytes are `[B, G, R, A]` — bind
    // them in that order, then repack as `0xRRGGBB`. (Getting this order wrong swaps
    // the red and blue channels, invisible on desaturated chrome but glaring on a
    // saturated statusline fill where R ≠ B.)
    let [b, g, r, a] = c.0.to_le_bytes();
    let lin = srgb_u32_to_linear((r as u32) << 16 | (g as u32) << 8 | b as u32);
    [lin[0], lin[1], lin[2], a as f32 / 255.0]
}

/// Convert a packed sRGB color to linear-space RGB floats (alpha handled by the
/// caller). The surface is sRGB, so clear/quad colors must be linear.
fn srgb_u32_to_linear(c: u32) -> [f32; 3] {
    let f = |b: u8| {
        let s = b as f32 / 255.0;
        if s <= 0.04045 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    [f((c >> 16) as u8), f((c >> 8) as u8), f(c as u8)]
}

// --- solid-color quad pipeline ---------------------------------------------

/// A rectangle to fill, in device pixels (origin top-left), with a premultiplied
/// linear-space RGBA color.
struct Quad {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: [f32; 4],
}

/// A minimal pipeline that draws solid-color rectangles. Vertices are built on
/// the CPU each frame in clip space (no instancing, no uniforms) and uploaded to
/// one growable vertex buffer — enough for the handful of fills a frame needs.
struct RectPipeline {
    pipeline: wgpu::RenderPipeline,
    buffer: wgpu::Buffer,
    capacity: u64,
    /// Vertices in the base layer (drawn first); they occupy `0..base_vertices`.
    base_vertices: u32,
    /// Total vertices uploaded; the overlay layer is `base_vertices..vertices`.
    vertices: u32,
}

const RECT_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};
@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) color: vec4<f32>) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.color = color;
    return out;
}
@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// Bytes per vertex: vec2 position + vec4 color.
const VERTEX_BYTES: u64 = (2 + 4) * 4;

impl RectPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nxvim-gui rect shader"),
            source: wgpu::ShaderSource::Wgsl(RECT_SHADER.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nxvim-gui rect layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nxvim-gui rect pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: VERTEX_BYTES,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: 8,
                            shader_location: 1,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let capacity = VERTEX_BYTES * 6 * 256;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nxvim-gui rect vertices"),
            size: capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            buffer,
            capacity,
            base_vertices: 0,
            vertices: 0,
        }
    }

    /// Build clip-space vertices for the `base` quads then the `overlay` quads —
    /// one contiguous buffer with the split recorded — and upload them, growing the
    /// buffer if needed. [`draw_base`](Self::draw_base) /
    /// [`draw_overlay`](Self::draw_overlay) then draw each range in turn.
    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        base: &[Quad],
        overlay: &[Quad],
        sw: f32,
        sh: f32,
    ) {
        let total = base.len() + overlay.len();
        let mut bytes: Vec<u8> = Vec::with_capacity(total * 6 * VERTEX_BYTES as usize);
        let mut push = |x: f32, y: f32, c: [f32; 4]| {
            // pixel (origin top-left) → clip space (origin center, y up).
            let cx = x / sw * 2.0 - 1.0;
            let cy = 1.0 - y / sh * 2.0;
            bytes.extend_from_slice(&cx.to_ne_bytes());
            bytes.extend_from_slice(&cy.to_ne_bytes());
            for ch in c {
                bytes.extend_from_slice(&ch.to_ne_bytes());
            }
        };
        for q in base.iter().chain(overlay) {
            let (x0, y0, x1, y1) = (q.x, q.y, q.x + q.w, q.y + q.h);
            push(x0, y0, q.color);
            push(x1, y0, q.color);
            push(x1, y1, q.color);
            push(x0, y0, q.color);
            push(x1, y1, q.color);
            push(x0, y1, q.color);
        }
        self.base_vertices = (base.len() * 6) as u32;
        self.vertices = (total * 6) as u32;
        if bytes.is_empty() {
            return;
        }
        if bytes.len() as u64 > self.capacity {
            self.capacity = (bytes.len() as u64).next_power_of_two();
            self.buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("nxvim-gui rect vertices"),
                size: self.capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.buffer, 0, &bytes);
    }

    /// Draw the base-layer quads (`0..base_vertices`).
    fn draw_base(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.draw_range(pass, 0, self.base_vertices);
    }

    /// Draw the overlay-layer quads (`base_vertices..vertices`).
    fn draw_overlay(&self, pass: &mut wgpu::RenderPass<'_>) {
        self.draw_range(pass, self.base_vertices, self.vertices);
    }

    /// Draw vertices `[start, end)` from the shared buffer (a no-op for an empty
    /// range).
    fn draw_range(&self, pass: &mut wgpu::RenderPass<'_>, start: u32, end: u32) {
        if end <= start {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.buffer.slice(..));
        pass.draw(start..end, 0..1);
    }
}
