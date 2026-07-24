//! GPU image rendering for `'imagepreview'` windows: decode an image file once,
//! upload it as a wgpu texture, and paint it as a centered, aspect-preserving
//! textured quad over the preview window's text body. The GUI shares the local
//! filesystem (like the TUI), so it decodes the bytes itself from the `image`
//! marker's path — the redraw frame carries only a *reference*, never the pixels
//! (the never-freeze invariant).
//!
//! Layout: the [`Renderer`](crate::render) collects an [`ImageDraw`] per preview
//! window during frame build (the window's text-body pixel rect plus the image's
//! disk reference), then calls [`ImageStore::prepare`] once — decoding/uploading
//! any new or changed-on-disk file and building this frame's quad vertices — and
//! [`ImageStore::draw`] inside the render pass to blit them over the base layer.

use std::collections::HashMap;
use std::ops::Range;

use nxvim_view::images::{decode_bytes, decode_file, ImageFetch, RemoteImages};
use nxvim_view::ImageData;
use tokio::sync::mpsc::UnboundedSender;

// The fetch request / remote byte cache / bounded decode helpers are the
// toolkit-neutral half shared with the TUI; they live in [`nxvim_view::images`].
// The decode cap there ([`nxvim_view::images::MAX_EDGE`]) also keeps every
// texture well under wgpu's `max_texture_dimension` (`max_dim` tightens it
// further on a smaller device limit).

/// One preview window's image draw request for a frame: the window's text-body
/// rect in physical pixels (origin top-left) and the image's disk reference. The
/// store fits the picture into `area` preserving aspect ratio and centers it.
pub(crate) struct ImageDraw {
    pub area: (f32, f32, f32, f32),
    pub image: ImageData,
}

/// One path's cache slot: the on-disk version it was decoded at (size + mtime-ms)
/// and the uploaded GPU texture, or `None` for a decode failure. Keeping the
/// version lets a changed-on-disk file re-upload (a stale or once-broken entry is
/// replaced) while an unchanged one — success or failure — is never re-read.
struct CacheEntry {
    version: (u64, u64),
    tex: Option<Tex>,
}

/// An uploaded image: its sampling bind group and pixel size (for the aspect fit).
struct Tex {
    bind_group: wgpu::BindGroup,
    px: (u32, u32),
}

/// What the renderer should paint for a preview window this frame.
pub(crate) enum ImageStatus {
    /// A texture is uploaded — blit it.
    Ready,
    /// A remote fetch is still in flight — show the loading placeholder.
    Loading,
    /// No usable texture (a decode failure, or a remote fetch that errored) — show
    /// the cannot-read placeholder.
    Failed,
}

/// The GUI's image renderer: the textured-quad pipeline, a path-keyed texture
/// cache, and this frame's quad vertices + per-draw cache keys (rebuilt each
/// frame in [`prepare`](Self::prepare), issued in [`draw`](Self::draw)).
pub(crate) struct ImageStore {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    cache: HashMap<String, CacheEntry>,

    vbuf: wgpu::Buffer,
    capacity: u64,
    /// Per-draw `(vertex range, path)` for this frame, in draw order. The path keys
    /// the cache for the bind group; a draw whose decode failed is omitted.
    draws: Vec<(Range<u32>, String)>,
    max_dim: u32,
    /// Out-of-band byte fetches for remote (daemon-session) previews (shared with
    /// the TUI; see [`RemoteImages`]).
    remote: RemoteImages,
    /// The sink the App's IO thread drains to issue `nxvim_image_read` requests; a
    /// reply comes back via [`ImageStore::deliver`].
    fetch_tx: UnboundedSender<ImageFetch>,
}

/// Bytes per vertex: vec2 clip-space position + vec2 texture UV.
const VERTEX_BYTES: u64 = (2 + 2) * 4;

const IMAGE_SHADER: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};
@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>) -> VsOut {
    var out: VsOut;
    out.pos = vec4<f32>(pos, 0.0, 1.0);
    out.uv = uv;
    return out;
}
@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
"#;

impl ImageStore {
    pub(crate) fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        max_dim: u32,
        fetch_tx: UnboundedSender<ImageFetch>,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("nxvim-gui image shader"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_SHADER.into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("nxvim-gui image bind layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("nxvim-gui image layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("nxvim-gui image pipeline"),
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
                            format: wgpu::VertexFormat::Float32x2,
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
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nxvim-gui image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let capacity = VERTEX_BYTES * 6 * 8;
        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("nxvim-gui image vertices"),
            size: capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            bind_layout,
            sampler,
            cache: HashMap::new(),
            vbuf,
            capacity,
            draws: Vec::new(),
            max_dim,
            remote: RemoteImages::new(),
            fetch_tx,
        }
    }

    /// Decode/upload any new or changed-on-disk image in `live`, and free the GPU
    /// texture of any cached path no longer shown. Called *before* the frame is
    /// built so [`failed`](Self::failed) is accurate the same frame — letting the
    /// renderer paint the `[image: …]` placeholder for a broken file immediately
    /// (a one-frame lag could otherwise never repaint, redraws being event-driven).
    pub(crate) fn ensure(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        live: &[&ImageData],
    ) {
        // Drop cache entries no shown image references, so a closed preview (or a
        // buffer that switched away from an image) frees its GPU texture and its
        // fetched remote bytes.
        let keep: std::collections::HashSet<&str> =
            live.iter().map(|im| im.path.as_str()).collect();
        self.cache.retain(|k, _| keep.contains(k.as_str()));
        self.remote.retain_paths(|k| keep.contains(k));

        for im in live {
            let path = im.path.as_str();
            let version = (im.size, im.mtime_ms);
            // A remote preview's bytes live on the daemon; make sure a fetch is in
            // flight (or already resolved) for this version before deciding to decode.
            if im.remote {
                if let Some(req) = self.remote.ensure_fetch(path, version) {
                    let _ = self.fetch_tx.send(req);
                }
            }
            // (Re)upload when there's no entry or the on-disk version moved (the
            // latter also retries a file whose earlier decode failed but was fixed).
            if self.cache.get(path).map(|e| e.version) != Some(version) {
                // The source bytes: a remote preview decodes the fetched bytes (and
                // skips this frame until they land — keeping any stale texture so a
                // reload doesn't flash the placeholder); a local preview reads the
                // shared disk.
                let decoded = if im.remote {
                    match self.remote.ready(path, version) {
                        Some(bytes) => decode_bytes(bytes, self.max_dim),
                        None => continue,
                    }
                } else {
                    decode_file(path, self.max_dim)
                };
                let tex = decoded.map(|img| {
                    let px = (img.width(), img.height());
                    let bind_group = self.upload(device, queue, &img);
                    Tex { bind_group, px }
                });
                self.cache
                    .insert(path.to_string(), CacheEntry { version, tex });
            }
        }
    }

    /// Receive an `nxvim_image_read` reply for a remote preview (routed from the IO
    /// thread): cache the bytes, or mark the fetch failed, so the next `ensure`
    /// decodes/falls-back. A reply for a version no longer requested is dropped (a
    /// newer fetch superseded it). The caller requests a repaint afterward.
    pub(crate) fn deliver(
        &mut self,
        path: String,
        version: (u64, u64),
        result: Result<Vec<u8>, String>,
    ) {
        self.remote.deliver(path, version, result);
    }

    /// Drop all cached image state — GPU textures and fetched remote bytes. Called on
    /// a `:connect` swap: the new session's files are unrelated to the old session's
    /// paths, so neither the textures nor the fetched bytes carry over.
    pub(crate) fn clear(&mut self) {
        self.cache.clear();
        self.remote.clear();
    }

    /// Build this frame's quad vertices from the already-decoded cache: each
    /// picture fit into its window's area (aspect-preserving, never upscaled past
    /// its natural pixel size, centered). A draw whose decode failed is skipped
    /// (the renderer drew the `[image: …]` placeholder for it). Call after
    /// [`ensure`](Self::ensure) and before [`draw`](Self::draw).
    pub(crate) fn build_quads(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        draws: &[ImageDraw],
        sw: f32,
        sh: f32,
    ) {
        self.draws.clear();
        let mut bytes: Vec<u8> = Vec::new();
        for d in draws {
            let path = d.image.path.as_str();
            let Some(tex) = self.cache.get(path).and_then(|e| e.tex.as_ref()) else {
                continue; // decode failed → renderer drew the text placeholder
            };
            let rect = centered_fit(d.area, tex.px);
            let start = (bytes.len() / VERTEX_BYTES as usize) as u32;
            push_quad(&mut bytes, rect, sw, sh);
            let end = (bytes.len() / VERTEX_BYTES as usize) as u32;
            self.draws.push((start..end, path.to_string()));
        }

        if bytes.is_empty() {
            return;
        }
        if bytes.len() as u64 > self.capacity {
            self.capacity = (bytes.len() as u64).next_power_of_two();
            self.vbuf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("nxvim-gui image vertices"),
                size: self.capacity,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.vbuf, 0, &bytes);
    }

    /// What the renderer should paint for `image` this frame. `Ready` when a texture
    /// is uploaded (blit it — including a *stale* one kept across a reload, so the
    /// picture doesn't flash to a placeholder while the new bytes fetch). Otherwise no
    /// texture: `Loading` while a remote fetch is still in flight, else `Failed` (a
    /// decode failure or an errored fetch → the text fallback). The store itself never
    /// paints text.
    pub(crate) fn status(&self, image: &ImageData) -> ImageStatus {
        if self
            .cache
            .get(image.path.as_str())
            .is_some_and(|e| e.tex.is_some())
        {
            return ImageStatus::Ready;
        }
        let version = (image.size, image.mtime_ms);
        if self.remote.is_loading(image.path.as_str(), version) {
            ImageStatus::Loading
        } else {
            ImageStatus::Failed
        }
    }

    /// Blit this frame's prepared image quads (each with its own texture bind
    /// group). Called inside the render pass, after the base-layer quads.
    pub(crate) fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.draws.is_empty() {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.vbuf.slice(..));
        for (range, path) in &self.draws {
            if let Some(tex) = self.cache.get(path).and_then(|e| e.tex.as_ref()) {
                pass.set_bind_group(0, &tex.bind_group, &[]);
                pass.draw(range.clone(), 0..1);
            }
        }
    }

    /// Upload `img` (RGBA8) as an sRGB texture and build its sampling bind group.
    /// The surface is sRGB and the bytes are sRGB-encoded, so an `Rgba8UnormSrgb`
    /// texture round-trips correctly: the sampler linearizes on read and the
    /// surface re-encodes on write.
    fn upload(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        img: &image::DynamicImage,
    ) -> wgpu::BindGroup {
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nxvim-gui image texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * w),
                rows_per_image: Some(h),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("nxvim-gui image bind group"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        })
    }
}

/// The largest aspect-preserving sub-rect of `area` (physical pixels) the image
/// fits in — never upscaled past its natural pixel size, matching the TUI's
/// `Resize::Fit` — centered within `area`. `px` is the image's pixel size.
fn centered_fit(area: (f32, f32, f32, f32), px: (u32, u32)) -> (f32, f32, f32, f32) {
    let (ax, ay, aw, ah) = area;
    let (iw, ih) = (px.0 as f32, px.1 as f32);
    if iw <= 0.0 || ih <= 0.0 || aw <= 0.0 || ah <= 0.0 {
        return area;
    }
    // Scale down to fit both dimensions; clamp to 1.0 so we never upscale.
    let scale = (aw / iw).min(ah / ih).min(1.0);
    let w = iw * scale;
    let h = ih * scale;
    (ax + (aw - w) / 2.0, ay + (ah - h) / 2.0, w, h)
}

/// Append the 6 vertices (two triangles) of a textured quad, converting pixel
/// coordinates (origin top-left) into clip space (origin center, y up) and mapping
/// the full `[0,1]²` UV range. Mirrors `RectPipeline::upload`'s winding.
fn push_quad(bytes: &mut Vec<u8>, rect: (f32, f32, f32, f32), sw: f32, sh: f32) {
    let (x, y, w, h) = rect;
    let (x0, y0, x1, y1) = (x, y, x + w, y + h);
    let mut v = |px: f32, py: f32, u: f32, t: f32| {
        let cx = px / sw * 2.0 - 1.0;
        let cy = 1.0 - py / sh * 2.0;
        bytes.extend_from_slice(&cx.to_ne_bytes());
        bytes.extend_from_slice(&cy.to_ne_bytes());
        bytes.extend_from_slice(&u.to_ne_bytes());
        bytes.extend_from_slice(&t.to_ne_bytes());
    };
    v(x0, y0, 0.0, 0.0);
    v(x1, y0, 1.0, 0.0);
    v(x1, y1, 1.0, 1.0);
    v(x0, y0, 0.0, 0.0);
    v(x1, y1, 1.0, 1.0);
    v(x0, y1, 0.0, 1.0);
}
