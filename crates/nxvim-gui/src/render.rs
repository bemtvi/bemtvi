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

use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache,
    TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use nxvim_view::{InlayHint, PanelData, StatusSegment, Style, View, WindowRegion, WindowView};
use winit::window::Window;

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
/// The diagnostic sign column's width in cells (vim's `signcolumn`), reserved at
/// the window's far left when [`WindowView::sign_column`] is set — left of the
/// number gutter. Mirrors the TUI's `SIGN_WIDTH`.
const SIGN_WIDTH: u16 = 2;
/// The multi-cursor accent (a warm amber): the active (primary) cursor's color in
/// MultiCursor placement mode, and the secondary cursors' underline tint in
/// insert/replace mode, so every multi-cursor decoration reads as one family.
/// Mirrors the TUI's `MULTICURSOR_ACCENT`.
const MULTICURSOR_ACCENT: u32 = 0xe5_c0_7b;

/// A cached shaped line: the glyphon buffer plus the frame it was last used in.
struct CacheEntry {
    buffer: Buffer,
    used: u64,
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
    pub bold: bool,
    pub italic: bool,
}

impl Seg {
    /// A plain run with no weight/slant — gutter numbers, status text, etc.
    pub fn plain(text: String, fg: u32) -> Self {
        Self {
            text,
            fg,
            bold: false,
            italic: false,
        }
    }
}

/// One piece of text to draw: a cache key (the shaped buffer), where to put it,
/// and the rect to clip it to (the whole surface for static text; the window's
/// text area for a scroll slide, so partially-scrolled lines clip at the edge).
struct TextItem {
    key: u64,
    x: f32,
    y: f32,
    color: Color,
    bounds: TextBounds,
}

/// An interpolated scroll-slide frame for the focused window, supplied by the
/// client clock each animation frame. `top`/`cursor` are fractional 0-based line
/// indices (the smoothness comes from *not* rounding them); the band
/// (`lines`/`numbers`/`highlights`, palette `styles`) starts at `base_line`, so
/// band entry `k` is buffer line `base_line + k`, drawn at sub-pixel
/// `y = (base_line + k - top) * cell_h`.
pub struct ScrollFrame<'a> {
    pub top: f32,
    pub cursor: f32,
    pub base_line: usize,
    pub lines: &'a [String],
    /// Per-row visual-selection spans for the band (aligned with `lines`), so the
    /// selection slides with the text. `None` rows carry no selection.
    pub selection: &'a [Option<(u16, u16)>],
    /// How to clip the selection's moving edge to the interpolated `cursor` as the
    /// slide grows: `Some(true)` extending down, `Some(false)` up, `None` for a
    /// pure scroll (cursor unmoved) where the full extent just slides.
    pub sel_clip: Option<bool>,
    pub numbers: &'a [Option<usize>],
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
    pub styles: &'a [Style],
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

    /// Configured font family name, or `None` for the system monospace. Used when
    /// shaping each line (and measuring the cell at startup).
    font_name: Option<String>,

    /// Device-pixel cell size, measured from the configured font once at startup.
    cell_w: f32,
    cell_h: f32,
    font_size: f32,
    line_height: f32,
    /// The window's scale factor (device pixels per logical pixel), kept so
    /// [`Renderer::set_font`] can rescale a new point size like `new` did.
    scale: f32,
}

impl Renderer {
    /// Build the renderer for `window`, rendering with `config`'s font family and
    /// size. Blocks on wgpu's async adapter/device requests via `pollster` (we are
    /// on the synchronous winit setup path).
    pub fn new(window: Arc<Window>, cfg: &GuiConfig) -> anyhow::Result<Self> {
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
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();

        let font_name = cfg.font.clone();
        let font_size = cfg.font_size * scale;
        let line_height = (font_size * LINE_SPACING).round();
        let family = font_name
            .as_deref()
            .map(Family::Name)
            .unwrap_or(Family::Monospace);
        let (cell_w, cell_h) = measure_cell(&mut font_system, family, font_size, line_height);

        let rects = RectPipeline::new(&device, format);

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
            font_name,
            cache: HashMap::new(),
            gen: 0,
            max_dim,
            cell_w,
            cell_h,
            font_size,
            line_height,
            scale,
        })
    }

    /// Re-shape with a new font family (`None` = system monospace) and point size,
    /// re-measuring the cell and dropping the shaped-line cache (its buffers were
    /// shaped at the old metrics). The caller then re-reports the grid (the cell
    /// size, hence `grid_size`, has changed) and repaints. Backs `:set guifont=…`.
    pub fn set_font(&mut self, font: Option<&str>, size_pt: f32) {
        self.font_name = font.map(str::to_string);
        self.font_size = size_pt * self.scale;
        self.line_height = (self.font_size * LINE_SPACING).round();
        let family = self
            .font_name
            .as_deref()
            .map(Family::Name)
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

    /// The measured cell size in physical pixels, for turning a trackpad's
    /// pixel-precise scroll delta into whole-line wheel notches.
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// Reconfigure the surface after a window resize. The size is clamped to the
    /// device's max texture dimension so a maximize/zoom onto a large hi-DPI
    /// display can't exceed it and abort in `Surface::configure`.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width.min(self.max_dim);
        self.config.height = height.min(self.max_dim);
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
        items
            .iter()
            .filter_map(|it| {
                cache.get(&it.key).map(|e| TextArea {
                    buffer: &e.buffer,
                    left: it.x,
                    top: it.y,
                    scale: 1.0,
                    bounds: it.bounds,
                    default_color: it.color,
                    custom_glyphs: &[],
                })
            })
            .collect()
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
        let mut quads: Vec<Quad> = Vec::new();
        let mut items: Vec<TextItem> = Vec::new();
        let mut overlay_quads: Vec<Quad> = Vec::new();
        let mut overlay_items: Vec<TextItem> = Vec::new();
        self.build_frame(
            view,
            scroll,
            doc_scroll,
            &mut quads,
            &mut items,
            &mut overlay_quads,
            &mut overlay_items,
        );
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
    ) {
        let (cols, total_rows) = self.grid_size();
        let cmd_row = total_rows.saturating_sub(1);

        // Chrome rows the windows area gives up: the tabline at the top (≥2 tabs),
        // the global status line and the panel at the bottom. The server already
        // sized the window rects to fit what's left.
        let tabline_rows = u16::from(!view.tabline.is_empty());
        let global_status_rows = u16::from(!view.global_status.is_empty());
        let panel_rows = view.panel.as_ref().map_or(0, |p| p.height + 1);

        // The permanent dock bands. Each open dock reserves its content extent plus
        // one separator cell toward the main area; the frame stacks as `[top dock]
        // [tabline][left|main|right][global status][bottom dock][panel][cmd]`. With
        // no dock open every band is 0 and this collapses to the pre-dock layout.
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
            .saturating_sub(panel_rows)
            .max(1);
        let main_x = left_band;
        let main_w = cols
            .saturating_sub(left_band)
            .saturating_sub(right_band)
            .max(1);
        let bottom_y = mid_y + mid_h + global_status_rows;
        // Each region's content-cell origin (the dock separator faces the main area,
        // so the bottom/right docks' content starts one cell past the band edge).
        let region_origin = |region: WindowRegion| -> (u16, u16) {
            match region {
                WindowRegion::Main => (main_x, mid_y),
                WindowRegion::DockLeft => (0, mid_y),
                WindowRegion::DockRight => (cols.saturating_sub(dr), mid_y),
                WindowRegion::DockTop => (0, 0),
                WindowRegion::DockBottom => (0, bottom_y + (bottom_band - db)),
            }
        };

        // The tabline on the row below the top dock.
        if tabline_rows > 0 {
            self.build_tabline(view, cols, quads, items);
        }

        // Tiled windows first; floats overlay them in a second pass so they sit on
        // top. Only the focused window slides; the rest paint from the live view.
        for win in view.windows.iter().filter(|w| !w.floating) {
            let (ox, oy) = region_origin(win.region);
            let rect = match win.rect {
                Some(r) => (ox + r.x, oy + r.y, r.width, r.height),
                None => (main_x, mid_y, main_w, mid_h),
            };
            let win_scroll = if win.focused { scroll } else { None };
            self.build_window(view, win, win_scroll, rect, quads, items);
        }

        // Separators between splits — thin grey lines, each offset by its region's
        // content origin like the windows they divide.
        let sep = srgb_to_color(style_bg(&view.status_line).unwrap_or(0x40_40_40));
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
        // The border line between each open dock and the main area.
        if dt > 0 {
            let (cx, cy) = self.cell_px(0, dt);
            line(cx, cy, self.cell_w * cols as f32, self.cell_h * 0.12);
        }
        if db > 0 {
            let (cx, cy) = self.cell_px(0, bottom_y);
            line(cx, cy, self.cell_w * cols as f32, self.cell_h * 0.12);
        }
        if dl > 0 {
            let (cx, cy) = self.cell_px(dl, mid_y);
            line(cx, cy, self.cell_w * 0.12, self.cell_h * mid_h as f32);
        }
        if dr > 0 {
            let (cx, cy) = self.cell_px(cols.saturating_sub(dr + 1), mid_y);
            line(cx, cy, self.cell_w * 0.12, self.cell_h * mid_h as f32);
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
            );
        }

        // The global status line (`laststatus=3`), docked just below the main band.
        if global_status_rows > 0 {
            let row = mid_y + mid_h;
            self.build_status_row(&view.global_status, (0, row), cols, view, quads, items);
        }

        // The bottom panel (`:messages`, `:ls`), claiming its rows above the
        // command line.
        if let Some(panel) = &view.panel {
            let top = cmd_row.saturating_sub(panel_rows);
            self.build_panel(panel, top, cols, view, quads, items);
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
        // layer and anchored the same way (the focused window's region origin).
        self.build_menu(view, focus_origin, overlay_quads, overlay_items);

        // The global command / message line on the reserved bottom row.
        self.build_cmdline(view, cmd_row, quads, items);
    }

    /// Paint one window: gutter numbers, text rows (syntax-colored), the visual
    /// selection and search highlights, its status bar, and — if focused — the
    /// cursor.
    fn build_window(
        &mut self,
        view: &View,
        win: &WindowView,
        scroll: Option<&ScrollFrame>,
        rect: (u16, u16, u16, u16),
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        // `rect` is the window's content rect in absolute screen cells, already
        // offset by the windows-area origin (and inset past a float's border).
        let (ox, oy, wcols, wrows) = rect;
        // Left columns: a 2-cell diagnostic sign column (vim's `signcolumn`, when
        // this buffer has diagnostics and signs are on), then the number gutter,
        // then the text — so the text origin shifts past both. Cursor, pmenu, and
        // mouse hit-test all derive from the same `text_x0` (see `pmenu_hit`).
        let sign_w = if win.sign_column { SIGN_WIDTH } else { 0 };
        let gutter = if win.number || win.relativenumber {
            win.number_width
        } else {
            0
        };
        let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
        let gutter_x0 = ox + sign_w;
        let text_x0 = ox + sign_w + gutter;
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
                // The cursor line tracks the interpolated slide, so relative numbers
                // stay in step with the moving text; the selection's moving edge is
                // clipped to the same line (see `sel_clip`).
                let cur_line0 = s.cursor.round() as usize; // 0-based interpolated cursor
                let current_line = cur_line0 + 1;
                for (k, raw) in s.lines.iter().enumerate() {
                    let row = (s.base_line + k) as f32 - s.top;
                    if row <= -1.0 || row >= text_rows as f32 {
                        continue; // fully outside the text area
                    }
                    let y = (oy as f32 + row) * self.cell_h;
                    let inlay = s.inlay_hints.get(k).map(Vec::as_slice).unwrap_or(&[]);
                    // Visual selection rides the slide. Its moving edge grows with the
                    // scroll: rows the interpolated cursor hasn't reached yet are not
                    // highlighted (the band carries the destination extent), so the
                    // selection extends together with the slide instead of flashing to
                    // full extent on frame 0. A pure scroll (`sel_clip == None`) slides
                    // the whole extent. The quad clamps to the text area vertically
                    // (quads aren't scissored) so a partial row cuts off at the edge.
                    if let Some(Some(span)) = s.selection.get(k) {
                        let line0 = s.base_line + k; // 0-based buffer line of this row
                        let hidden = match s.sel_clip {
                            Some(true) => line0 > cur_line0,
                            Some(false) => line0 < cur_line0,
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
                    if gutter > 0 {
                        if let Some(Some(n)) = s.numbers.get(k) {
                            let pos = (gutter_x0 as f32 * self.cell_w, y);
                            self.push_gutter(items, (*n, current_line), win, view, pos, clip);
                        }
                    }
                    let hl = s.highlights.get(k).map(Vec::as_slice).unwrap_or(&[]);
                    let mut segments =
                        row_segments(&display, hl, s.styles, fg, slide_bg, win.leftcol);
                    // Splice the band row's inlay hints in, like the settled path, so
                    // they slide with the text instead of vanishing during the slide.
                    segments = splice_inlay(segments, inlay, win.leftcol, s.styles);
                    let x = text_x0.saturating_sub(win.leftcol) as f32 * self.cell_w;
                    self.push_text(items, &segments, (x, y), fg, clip);
                }
            }
            // Settled: paint the live viewport with selection/search overlays.
            None => {
                let full = self.full_bounds();
                let sel_bg = style_bg(&view.visual).unwrap_or(0x33_47_5b);
                let search_bg = style_bg(&view.search_style).unwrap_or(0x6a_5a_1a);
                let normal_bg = style_bg(&view.normal).unwrap_or(DEFAULT_BG);
                for (i, raw) in win.lines.iter().enumerate() {
                    let row = oy as usize + i;
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
                            let color = id
                                .and_then(|id| view.styles.get(id))
                                .and_then(|st| st.fg)
                                .unwrap_or_else(|| severity_color(*severity));
                            let text = pad_to_width(glyph, sign_w as usize);
                            self.push_plain(
                                items,
                                &text,
                                self.cell_px(ox, row as u16),
                                color,
                                full,
                            );
                        }
                    }

                    // Gutter number for this row, honoring number/relativenumber
                    // and the cursor-line highlight.
                    if gutter > 0 {
                        if let Some(Some(n)) = win.numbers.get(i) {
                            let pos = self.cell_px(gutter_x0, row as u16);
                            self.push_gutter(items, (*n, win.cursor_line), win, view, pos, full);
                        }
                    }

                    // The text itself, syntax-colored from the row's highlights, with
                    // the inlay hints spliced in at their anchor columns.
                    let hl = win.highlights.get(i).map(Vec::as_slice).unwrap_or(&[]);
                    // Reverse fills (a foreground-colored quad behind the inverted
                    // glyph) go under the text; underline/strikethrough rules go over
                    // it. Both walk the same highlight spans (see the methods).
                    self.push_reverse_fills(quads, win, view, text_x0, row, hl, inlay);
                    let mut segments =
                        row_segments(&display, hl, &view.styles, fg, normal_bg, win.leftcol);
                    segments = splice_inlay(segments, inlay, win.leftcol, &view.styles);
                    let pos = self.cell_px(text_x0.saturating_sub(win.leftcol), row as u16);
                    self.push_text(items, &segments, pos, fg, full);
                    self.push_attr_rules(quads, win, view, text_x0, row, hl, inlay);

                    // LSP diagnostic underlines, painted last so they survive over
                    // the syntax/selection: a thin colored rule under the cells.
                    if let Some(diags) = win.diagnostics.get(i) {
                        for (s, e, severity, id) in diags {
                            let color = id
                                .and_then(|id| view.styles.get(id))
                                .and_then(|st| st.sp.or(st.fg))
                                .unwrap_or_else(|| severity_color(*severity));
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
                                let color = id
                                    .and_then(|id| view.styles.get(id))
                                    .and_then(|st| st.fg)
                                    .unwrap_or_else(|| severity_color(*severity));
                                self.push_plain(
                                    items,
                                    &shown,
                                    self.cell_px(start, row as u16),
                                    color,
                                    full,
                                );
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

        // The cursor lives only in the focused window — but a focused panel or the
        // command line owns it instead, so suppress the window cursor while either
        // is active (it reappears on the command line / panel). While sliding it
        // tracks the interpolated cursor line so it moves with the text.
        if win.focused && view.panel.is_none() && !view.command_mode {
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
                    inlay_shift(row_inlay, win.leftcol, win.cursor_screen_col, true)
                }
                Some(s) => {
                    let idx = (s.cursor.round() as usize).saturating_sub(s.base_line);
                    let row_inlay = s.inlay_hints.get(idx).map(Vec::as_slice).unwrap_or(&[]);
                    inlay_shift(row_inlay, win.leftcol, win.cursor_screen_col, true)
                }
            };
            let cx = text_x0 + win.cursor_screen_col.saturating_sub(win.leftcol) + cur_shift;
            let px = cx as f32 * self.cell_w;
            let py = match scroll {
                Some(s) => (oy as f32 + (s.cursor - s.top)) * self.cell_h,
                None => (oy + win.cursor_row) as f32 * self.cell_h,
            };
            // In MultiCursor placement mode the active (primary) cursor wears the
            // multi-cursor accent, distinct from the secondaries, so it reads as
            // "the one dropping cursors" — mirroring the TUI.
            let cursor_color = if view.is_multicursor() {
                MULTICURSOR_ACCENT
            } else {
                style_fg(&view.normal).unwrap_or(DEFAULT_FG)
            };
            let (w, alpha) = if view.is_insert() {
                (self.cell_w * 0.15, 0.9)
            } else {
                (self.cell_w, 0.5) // block, semi-transparent so the glyph shows
            };
            let mut c = srgb_to_color_rgba(cursor_color, alpha);
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
            c[3] = alpha;
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
                let mut c = srgb_to_color_rgba(fg, 0.5);
                c[3] = 0.5;
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
        let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
        let pos = self.cell_px(0, cmd_row);
        let text = if view.command_mode || !view.cmdline.is_empty() {
            if !view.cmdline_prompt.is_empty() {
                format!("{}{}", view.cmdline_prompt, view.cmdline)
            } else {
                format!("{}{}", view.cmdline_prefix, view.cmdline)
            }
        } else {
            view.message.clone()
        };
        let full = self.full_bounds();
        self.push_plain(items, &text, pos, fg, full);

        // The command-line cursor: a semi-transparent block past the leading prompt
        // (a single prefix char, or the multi-char `vim.ui.input` label).
        if view.command_mode {
            let prompt_width = if view.cmdline_prompt.is_empty() {
                1 // `:` / `/` / `?`
            } else {
                view.cmdline_prompt.chars().count() as u16
            };
            let col = prompt_width + view.cmdline_cursor as u16;
            let (px, py) = self.cell_px(col, cmd_row);
            let cursor_color = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
            let mut c = srgb_to_color_rgba(cursor_color, 0.5);
            c[3] = 0.5;
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
    /// segments when set, else the built-in cells (` {count} {label}{+} `, the
    /// active cell reverse-video) — the GUI port of the TUI's `render_tabline`.
    fn build_tabline(
        &mut self,
        view: &View,
        cols: u16,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let base_bg = style_bg(&view.status_line).unwrap_or(0x1a_1a_1a);
        let base_fg = style_fg(&view.status_line).unwrap_or(DEFAULT_FG);
        self.fill_row(quads, 0, cols, base_bg);

        // A custom `'tabline'` already rendered to styled segments: paint verbatim.
        if !view.tabline.is_empty() && !view.tabline_segments.is_empty() {
            self.paint_segments(&view.tabline_segments, 0, 0, base_fg, quads, items);
            return;
        }

        let mut col = 0u16;
        for (i, tab) in view.tabline.iter().enumerate() {
            let count = if tab.window_count > 1 {
                format!("{} ", tab.window_count)
            } else {
                String::new()
            };
            let modified = if tab.modified { "+" } else { "" };
            let text = format!(" {count}{}{modified} ", tab.label);
            let w = text.chars().count() as u16;
            // The active cell is reverse-video: the status fg becomes its ground.
            let fg = if i == view.current_tab {
                if w > 0 {
                    self.fill_cells(quads, col, 0, w, base_fg);
                }
                base_bg
            } else {
                base_fg
            };
            let pos = self.cell_px(col, 0);
            let full = self.full_bounds();
            self.push_plain(items, &text, pos, fg, full);
            col = col.saturating_add(w);
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
    /// text in its own foreground (falling back to `base_fg`). Char count is the
    /// cell advance (exact for the ASCII/box-drawing text these segments carry).
    /// Shared by the tabline and the status rows.
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
        for (text, style) in segments {
            let w = text.chars().count() as u16;
            if let Some(bg) = style.as_ref().and_then(|s| s.bg) {
                if w > 0 {
                    self.fill_cells(quads, col, row, w, bg);
                }
            }
            let fg = style.as_ref().and_then(|s| s.fg).unwrap_or(base_fg);
            let pos = self.cell_px(col, row);
            let full = self.full_bounds();
            self.push_plain(items, text, pos, fg, full);
            col = col.saturating_add(w);
        }
    }

    /// Paint the bottom panel (`:messages`, `:ls`): an opaque background, a
    /// `─ Title ───[X]─` top bar, then the content rows with the selected (cursor)
    /// entry reverse-highlighted across the full width. The GUI port of the TUI's
    /// `render_panel`; the focused cursor sits here, so the window cursor is
    /// suppressed (see `build_window`).
    fn build_panel(
        &mut self,
        panel: &PanelData,
        top: u16,
        cols: u16,
        view: &View,
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let bg = style_bg(&view.normal).unwrap_or(DEFAULT_BG);
        let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
        let border = lighten(bg, 0x30);
        // Opaque fill behind the whole panel (title bar + content rows).
        let (px, py) = self.cell_px(0, top);
        quads.push(Quad {
            x: px,
            y: py,
            w: self.cell_w * cols as f32,
            h: self.cell_h * (panel.height + 1) as f32,
            color: color_to_rgba(srgb_to_color(bg)),
        });
        // A thin top rule marks the panel's border edge.
        quads.push(Quad {
            x: px,
            y: py,
            w: self.cell_w * cols as f32,
            h: (self.cell_h * 0.10).max(1.0),
            color: color_to_rgba(srgb_to_color(border)),
        });
        // Title on the bar, and the click-to-close `[X]` at the right.
        let full = self.full_bounds();
        let title = format!(" {} ", panel.title);
        self.push_plain(items, &title, self.cell_px(0, top), fg, full);
        let close = "[X]";
        let cx = cols.saturating_sub(close.chars().count() as u16);
        self.push_plain(items, close, self.cell_px(cx, top), fg, full);

        // Content rows; the selected (possibly word-wrapped) entry is highlighted
        // across its whole span so a wrapped entry still reads as one focused line.
        let content_top = top + 1;
        let cursor_end = panel.cursor_row.saturating_add(panel.cursor_span.max(1));
        for r in 0..panel.height {
            let row = content_top + r;
            let selected = r >= panel.cursor_row && r < cursor_end;
            if selected {
                self.fill_row(quads, row, cols, fg);
            }
            if let Some(text) = panel.lines.get(r as usize) {
                let tfg = if selected { bg } else { fg };
                self.push_plain(items, text, self.cell_px(0, row), tfg, full);
            }
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
    ) {
        let Some(r) = win.rect else {
            return; // a float always carries a rect
        };
        let ox = origin.0 + r.x;
        let oy = origin.1 + r.y;
        let bg = lighten(style_bg(&view.normal).unwrap_or(DEFAULT_BG), 0x10);
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
            Some(_) => {
                let border = lighten(bg, 0x30);
                self.box_frame(quads, ox, oy, r.width, r.height, border);
                if let Some(title) = &win.title {
                    let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);
                    let t = format!(" {title} ");
                    let full = self.full_bounds();
                    self.push_plain(items, &t, self.cell_px(ox + 1, oy), fg, full);
                }
                (
                    ox + 1,
                    oy + 1,
                    r.width.saturating_sub(2),
                    r.height.saturating_sub(2),
                )
            }
            None => (ox, oy, r.width, r.height),
        };
        self.build_window(view, win, None, inner, quads, items);
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
        let sign_w = if win.sign_column { SIGN_WIDTH } else { 0 };
        let gutter = if win.number || win.relativenumber {
            win.number_width
        } else {
            0
        };
        let text_x0 = wx + sign_w + gutter;

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
        self.fill_box(quads, (bx, by, box_w, box_h), popup_bg, border);

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
            self.fill_box(quads, (dx, by, dbw, dbh), popup_bg, border);
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
        quads: &mut Vec<Quad>,
        items: &mut Vec<TextItem>,
    ) {
        let Some(menu) = &view.menu else {
            return;
        };
        let Some(win) = view.focused() else {
            return;
        };
        // The focused window's text-inner origin in screen cells (rect + gutter),
        // identical to the popup's anchor derivation.
        let (mut wx, mut wy) = match win.rect {
            Some(r) => (origin.0 + r.x, origin.1 + r.y),
            None => (origin.0, origin.1),
        };
        if win.floating && win.border.is_some() {
            wx += 1;
            wy += 1;
        }
        let sign_w = if win.sign_column { SIGN_WIDTH } else { 0 };
        let gutter = if win.number || win.relativenumber {
            win.number_width
        } else {
            0
        };
        let text_x0 = wx + sign_w + gutter;

        let popup_bg = lighten(style_bg(&view.normal).unwrap_or(DEFAULT_BG), 0x14);
        let border = lighten(popup_bg, 0x30);
        let sel_bg = lighten(popup_bg, 0x28);
        let fg = style_fg(&view.normal).unwrap_or(DEFAULT_FG);

        let bx = text_x0 + menu.col;
        let by = wy + menu.row;
        let box_w = menu.width + 2;
        let box_h = menu.height + 2;
        self.fill_box(quads, (bx, by, box_w, box_h), popup_bg, border);

        let rows = menu.height as usize;
        let start = pmenu_start(Some(menu.selected), rows);
        let full = self.full_bounds();
        for r in 0..menu.height {
            let idx = start + r as usize;
            let Some(label) = menu.items.get(idx) else {
                continue;
            };
            let cx = bx + 1;
            let row = by + 1 + r;
            if idx == menu.selected {
                self.fill_cells(quads, cx, row, menu.width, sel_bg);
            }
            let text = pmenu_row(label, "", menu.width as usize);
            self.push_plain(items, &text, self.cell_px(cx, row), fg, full);
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

    /// Fill a `w`×`h`-cell box at `(x, y)` (`rect`) with `bg`, then outline it with
    /// a thin `border` frame — the opaque overlay panels (pmenu, doc preview) sit in.
    fn fill_box(&self, quads: &mut Vec<Quad>, rect: (u16, u16, u16, u16), bg: u32, border: u32) {
        let (x, y, w, h) = rect;
        let (px, py) = self.cell_px(x, y);
        quads.push(Quad {
            x: px,
            y: py,
            w: self.cell_w * w as f32,
            h: self.cell_h * h as f32,
            color: color_to_rgba(srgb_to_color(bg)),
        });
        self.box_frame(quads, x, y, w, h, border);
    }

    /// Draw a thin `border` frame around a `w`×`h`-cell box at `(x, y)` (four edge
    /// quads), leaving the interior untouched — the float / popup outline.
    fn box_frame(&self, quads: &mut Vec<Quad>, x: u16, y: u16, w: u16, h: u16, border: u32) {
        let (px, py) = self.cell_px(x, y);
        let pw = self.cell_w * w as f32;
        let ph = self.cell_h * h as f32;
        let t = (self.cell_w * 0.12).max(1.0);
        let c = color_to_rgba(srgb_to_color(border));
        let edges = [
            (px, py, pw, t),          // top
            (px, py + ph - t, pw, t), // bottom
            (px, py, t, ph),          // left
            (px + pw - t, py, t, ph), // right
        ];
        for (x, y, w, h) in edges {
            quads.push(Quad {
                x,
                y,
                w,
                h,
                color: c,
            });
        }
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
        let (s, e) = span;
        let start = base + s.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, s, true);
        let end = base + e.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, e, false);
        if end <= start {
            return;
        }
        let (px, py) = self.cell_px(start, row as u16);
        let h = (self.cell_h * 0.08).max(1.0);
        quads.push(Quad {
            x: px,
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
        let (s, e) = span;
        let start = base + s.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, s, true);
        let end = base + e.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, e, false);
        if end <= start {
            return;
        }
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
        let (s, e) = span;
        let start = base + s.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, s, true);
        let end = base + e.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, e, false);
        if end <= start {
            return;
        }
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
        let (s, e) = span;
        let start = base + s.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, s, true);
        let end = base + e.saturating_sub(leftcol) + inlay_shift(inlay, leftcol, e, false);
        if end <= start {
            return;
        }
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

    /// Pixel origin of cell `(col, row)`.
    fn cell_px(&self, col: u16, row: u16) -> (f32, f32) {
        (col as f32 * self.cell_w, row as f32 * self.cell_h)
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
        let color = if n == current_line {
            style_fg(&view.cursor_line_nr).unwrap_or(DEFAULT_FG)
        } else {
            style_fg(&view.line_nr).unwrap_or(DEFAULT_LINE_NR)
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
        let key = self.ensure(segments, default_fg);
        items.push(TextItem {
            key,
            x: pos.0,
            y: pos.1,
            color: srgb_to_color(default_fg),
            bounds,
        });
    }

    /// Ensure a shaped buffer for `segments` is cached, returning its key. A hit
    /// just refreshes the entry's frame stamp — the whole point: no reshaping for
    /// a line whose content hasn't changed.
    fn ensure(&mut self, segments: &[Seg], default_fg: u32) -> u64 {
        let key = line_key(segments, default_fg);
        if let Some(e) = self.cache.get_mut(&key) {
            e.used = self.gen;
            return key;
        }
        let buffer = self.shape_segments(segments);
        self.cache.insert(
            key,
            CacheEntry {
                buffer,
                used: self.gen,
            },
        );
        key
    }

    /// Shape `segments` into a fresh glyphon buffer (the expensive op the cache
    /// exists to avoid repeating). Each run carries its own color and — via a bold
    /// or italic face — its weight/slant, so `b`/`i` highlight attributes select a
    /// real heavier/slanted glyph (cosmic-text synthesizes one if the family lacks it).
    fn shape_segments(&mut self, segments: &[Seg]) -> Buffer {
        // Family borrows `font_name`; the buffer borrows `font_system` — disjoint
        // fields, so the two borrows coexist.
        let family = self
            .font_name
            .as_deref()
            .map(Family::Name)
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
                attrs = attrs.style(glyphon::Style::Italic);
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

/// Lighten a packed `0xRRGGBB` color by adding `d` to each channel (saturating) —
/// how the overlay surfaces (floats, popups, their borders) lift a region off the
/// editor background, which truecolor has no reverse-video shortcut for.
fn lighten(c: u32, d: u8) -> u32 {
    let f = |b: u8| b.saturating_add(d) as u32;
    (f((c >> 16) as u8) << 16) | (f((c >> 8) as u8) << 8) | f(c as u8)
}

/// Headless geometry of the completion popup for a `cols`-wide grid showing
/// `view`, mirroring [`Renderer::build_pmenu`]'s layout so the event loop can
/// hit-test the mouse against the painted box. `None` when no popup is drawn.
///
/// Like the TUI's `pmenu_geometry`/`pmenu_doc_geometry`, this is a parallel pure
/// definition of the same layout — keep it in step with `build_pmenu`.
pub(crate) struct PmenuHit {
    /// The clickable item area inside the border: `(x, y, w, h)` in cells.
    pub item: (u16, u16, u16, u16),
    /// First visible item index (the list's scroll offset); a click on the item
    /// area's row `r` chooses item `start + (r - item.y)`.
    pub start: usize,
    /// The doc preview box `(x, y, w, h)` and its largest client-side scroll, when
    /// a preview is shown — the wheel scrolls the docs while the pointer is over it.
    pub doc: Option<(u16, u16, u16, u16, u16)>,
}

/// Compute [`PmenuHit`] from the view and grid width, replicating `build_pmenu`.
pub(crate) fn pmenu_hit(view: &View, cols: u16) -> Option<PmenuHit> {
    let pmenu = view.pmenu.as_ref()?;
    let win = view.focused()?;
    // The focused window's text-inner origin (windows-area origin + rect + float
    // border inset + gutter), exactly as `build_pmenu` derives it.
    let tabline_rows = u16::from(!view.tabline.is_empty());
    let (mut wx, mut wy) = match win.rect {
        Some(r) => (r.x, tabline_rows + r.y),
        None => (0, tabline_rows),
    };
    if win.floating && win.border.is_some() {
        wx += 1;
        wy += 1;
    }
    let sign_w = if win.sign_column { SIGN_WIDTH } else { 0 };
    let gutter = if win.number || win.relativenumber {
        win.number_width
    } else {
        0
    };
    let text_x0 = wx + sign_w + gutter;

    let bx = text_x0 + pmenu.col;
    let by = wy + pmenu.row;
    let box_w = pmenu.width + 2;
    let item = (bx + 1, by + 1, pmenu.width, pmenu.height);
    let start = pmenu_start(pmenu.selected, pmenu.height as usize);

    // The doc preview box, when the selected item carries docs (same clamps and
    // left/right placement as `build_pmenu`).
    let doc = (!pmenu.doc.is_empty()).then(|| {
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
        let dx = if bx + box_w + dbw <= cols {
            bx + box_w
        } else {
            bx.saturating_sub(dbw)
        };
        let max_scroll = (pmenu.doc.len() as u16).saturating_sub(dch);
        (dx, by, dbw, dbh, max_scroll)
    });

    Some(PmenuHit { item, start, doc })
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
                // Reverse swaps fg/bg: the glyph takes the style's background (or
                // the editor's `Normal` bg) and a foreground-colored quad behind it
                // is painted by `push_reverse_fills`, so the run reads inverted.
                let color = if st.reverse {
                    st.bg.unwrap_or(normal_bg)
                } else {
                    st.fg.unwrap_or(fg)
                };
                segments.push(Seg {
                    text,
                    fg: color,
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

/// 0xRRGGBB → opaque glyphon [`Color`].
fn srgb_to_color(c: u32) -> Color {
    Color::rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}
fn srgb_to_color_rgba(c: u32, _alpha: f32) -> [f32; 4] {
    let lin = srgb_u32_to_linear(c);
    [lin[0], lin[1], lin[2], 1.0]
}
fn color_to_rgba(c: Color) -> [f32; 4] {
    // glyphon Color is sRGB bytes; our quad pipeline targets an sRGB surface, so
    // convert to linear (the GPU applies the sRGB encode on store).
    let [r, g, b, a] = c.0.to_le_bytes();
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
