//! Draws the terminal grid with wgpu: one instanced quad per rectangle (backgrounds, underlines,
//! cursor) and per glyph, all in a single render pass.

use std::mem::size_of;
use std::ops::Range;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use libghostty_vt::render::{CellIterator, Colors, CursorVisualStyle, RowIterator, Snapshot};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{RgbColor, StyleColor, Underline};
use libghostty_vt::{RenderState, Terminal};
use winit::window::Window;

use crate::config::Config;
use crate::glyphs::{GlyphCache, GlyphKey};
use crate::layout::Rect;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RectInstance {
    pos: [f32; 2],
    size: [f32; 2],
    color: [u8; 4],
}

impl RectInstance {
    fn new(x: u32, y: u32, width: u32, height: u32, color: RgbColor) -> Self {
        Self {
            pos: [x as f32, y as f32],
            size: [width as f32, height as f32],
            color: rgba(color),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GlyphInstance {
    pos: [f32; 2],
    size: [f32; 2],
    atlas: [f32; 2],
    color: [u8; 4],
}

/// A pane's instances within the frame: their ranges in the three vectors and the area they are
/// clipped to. Each pane is drawn in full (backgrounds, glyphs, and decorations) before the next
/// one, so whatever is on top completely covers what is underneath.
#[derive(Clone)]
struct Batch {
    scissor: Rect,
    backgrounds: Range<u32>,
    glyphs: Range<u32>,
    overlays: Range<u32>,
}

/// A frame's instances. The vectors are reused across frames to avoid allocating every time.
#[derive(Default)]
struct Frame {
    backgrounds: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    overlays: Vec<RectInstance>,
    batches: Vec<Batch>,
    text: String,
}

impl Frame {
    fn clear(&mut self) {
        self.backgrounds.clear();
        self.glyphs.clear();
        self.overlays.clear();
        self.batches.clear();
    }

    /// How many instances of each kind there are so far
    fn lengths(&self) -> (u32, u32, u32) {
        (
            self.backgrounds.len() as u32,
            self.glyphs.len() as u32,
            self.overlays.len() as u32,
        )
    }
}

/// A pane to draw: its terminal, the libghostty state used for drawing, and where it goes
pub struct PaneView<'a> {
    pub terminal: &'a Terminal<'static, 'static>,
    pub render_state: &'a mut RenderState<'static>,
    /// Rectangle in physical pixels, padding included
    pub rect: Rect,
    /// The cursor is drawn active (and blinks if the program asks); otherwise hollow and steady
    pub focused: bool,
    /// The text is dimmed (`panes.inactive_dim`): an unfocused pane when several are visible
    pub dim: bool,
    /// The focused-pane border is drawn around it
    pub frame: bool,
    /// The prefix is pressed: the border takes the prefix color
    pub leader: bool,
    /// Floating pane: covers what is underneath with an opaque background and always has a border
    pub floating: bool,
}

/// Colors for selected text (`None` = invert the screen colors)
#[derive(Clone, Copy, Default)]
struct SelectionColors {
    foreground: Option<RgbColor>,
    background: Option<RgbColor>,
}

impl From<&Config> for SelectionColors {
    fn from(settings: &Config) -> Self {
        Self {
            foreground: settings.colors.selection_foreground.map(|color| color.0),
            background: settings.colors.selection_background.map(|color| color.0),
        }
    }
}

/// How panes are told apart (`None` = derive it from the theme colors)
#[derive(Clone, Copy, Default)]
struct PaneStyle {
    /// Divider color
    border: Option<RgbColor>,
    /// Border color of the focused pane
    focus: Option<RgbColor>,
    /// Border color while the prefix is pressed
    leader: Option<RgbColor>,
    /// How much the text of unfocused panes is dimmed (0 to 1)
    inactive_dim: f32,
}

impl From<&Config> for PaneStyle {
    fn from(settings: &Config) -> Self {
        Self {
            border: settings.panes.border_color.map(|color| color.0),
            focus: settings.panes.focus_color.map(|color| color.0),
            leader: settings.panes.leader_color.map(|color| color.0),
            inactive_dim: settings.panes.inactive_dim,
        }
    }
}

#[derive(Clone, Copy)]
struct Cursor {
    col: u16,
    row: u16,
    style: CursorVisualStyle,
    color: RgbColor,
}

/// Vertex buffer that grows (in powers of two) when the frame needs more space
struct GrowBuffer {
    buffer: wgpu::Buffer,
    size: u64,
    label: &'static str,
}

impl GrowBuffer {
    fn new(device: &wgpu::Device, label: &'static str) -> Self {
        let size = 64 * 1024;
        Self {
            buffer: Self::allocate(device, label, size),
            size,
            label,
        }
    }

    fn allocate(device: &wgpu::Device, label: &'static str, size: u64) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Writes the chunks one after another
    fn write(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, parts: &[&[u8]]) {
        let total: u64 = parts.iter().map(|part| part.len() as u64).sum();
        if total > self.size {
            self.size = total.next_power_of_two();
            self.buffer = Self::allocate(device, self.label, self.size);
        }
        let mut offset = 0;
        for part in parts.iter().filter(|part| !part.is_empty()) {
            queue.write_buffer(&self.buffer, offset, part);
            offset += part.len() as u64;
        }
    }
}

pub struct Renderer {
    window: Arc<Window>,
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    globals: wgpu::Buffer,
    rect_pipeline: wgpu::RenderPipeline,
    rect_bind_group: wgpu::BindGroup,
    rect_buffer: GrowBuffer,
    glyph_pipeline: wgpu::RenderPipeline,
    glyph_bind_group: wgpu::BindGroup,
    glyph_buffer: GrowBuffer,
    cache: GlyphCache,
    /// Padding in logical pixels (from the configuration) and in physical pixels at the current
    /// scale
    padding_logical: f64,
    padding: u32,
    selection: SelectionColors,
    panes: PaneStyle,
    frame: Frame,
    // libghostty iterators used only for drawing (re-pointed for each pane)
    row_iter: RowIterator<'static>,
    cell_iter: CellIterator<'static>,
}

impl Renderer {
    pub fn new(window: Arc<Window>, settings: &Config) -> Result<Self, String> {
        let scale_factor = window.scale_factor();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| e.to_string())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| e.to_string())?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| e.to_string())?;

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("the surface is not compatible with the adapter")?;
        // Terminal colors are already sRGB: they are written as-is, without conversion
        let capabilities = surface.get_capabilities(&adapter);
        if let Some(format) = capabilities.formats.iter().copied().find(|f| !f.is_srgb()) {
            config.format = format;
        }
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&device, &config);

        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: size_of::<[f32; 4]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(
            &globals,
            0,
            bytemuck::bytes_of(&[config.width as f32, config.height as f32, 0.0, 0.0]),
        );

        let cache = GlyphCache::new(&device, queue.clone(), scale_factor, &settings.font)?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("terminal"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let rect_attributes =
            wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Unorm8x4];
        let rect_pipeline = create_pipeline(
            &device,
            &shader,
            config.format,
            ("vs_rect", "fs_rect"),
            wgpu::VertexBufferLayout {
                array_stride: size_of::<RectInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &rect_attributes,
            },
        );
        let glyph_attributes = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Unorm8x4
        ];
        let glyph_pipeline = create_pipeline(
            &device,
            &shader,
            config.format,
            ("vs_glyph", "fs_glyph"),
            wgpu::VertexBufferLayout {
                array_stride: size_of::<GlyphInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &glyph_attributes,
            },
        );

        // Layouts are inferred from the shader: the rectangle one only sees `globals`
        let rect_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rectangles"),
            layout: &rect_pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let atlas = cache.atlas_view();
        let glyph_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyphs"),
            layout: &glyph_pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas),
                },
            ],
        });

        Ok(Self {
            rect_buffer: GrowBuffer::new(&device, "rectangles"),
            glyph_buffer: GrowBuffer::new(&device, "glyphs"),
            padding_logical: settings.padding,
            padding: padding_for(settings.padding, scale_factor),
            selection: SelectionColors::from(settings),
            panes: PaneStyle::from(settings),
            frame: Frame::default(),
            row_iter: RowIterator::new().map_err(|e| e.to_string())?,
            cell_iter: CellIterator::new().map_err(|e| e.to_string())?,
            window,
            instance,
            surface,
            device,
            queue,
            config,
            globals,
            rect_pipeline,
            rect_bind_group,
            glyph_pipeline,
            glyph_bind_group,
            cache,
        })
    }

    /// Size of a cell in physical pixels
    pub fn cell_size(&self) -> (u32, u32) {
        (self.cache.cell_width, self.cache.cell_height)
    }

    /// Padding around the grid in physical pixels
    pub fn padding(&self) -> u32 {
        self.padding
    }

    /// Columns and rows that fit in a window of that size (in physical pixels)
    pub fn grid_size(&self, width: u32, height: u32) -> (u16, u16) {
        let cols = width.saturating_sub(2 * self.padding) / self.cache.cell_width;
        let rows = height.saturating_sub(2 * self.padding) / self.cache.cell_height;
        (
            cols.clamp(1, u16::MAX.into()) as u16,
            rows.clamp(1, u16::MAX.into()) as u16,
        )
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        // wgpu does not support 0×0 surfaces (a minimized window reports that size)
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&[width as f32, height as f32, 0.0, 0.0]),
        );
    }

    /// Applies the font and padding from a new configuration. If the font cannot be loaded, it
    /// returns the error without changing anything.
    pub fn reconfigure(&mut self, settings: &Config) -> Result<(), String> {
        self.cache.set_font(&settings.font)?;
        self.padding_logical = settings.padding;
        self.padding = padding_for(settings.padding, self.window.scale_factor());
        self.selection = SelectionColors::from(settings);
        self.panes = PaneStyle::from(settings);
        Ok(())
    }

    /// Thickness, in pixels, of the dividers between panes
    pub fn divider_thickness(&self) -> u32 {
        self.cache.thickness.max(1)
    }

    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.cache.set_scale_factor(scale_factor);
        self.padding = padding_for(self.padding_logical, scale_factor);
    }

    /// Draws a frame with the given panes, bottom to top. `blink_visible` is the current blink
    /// phase. Returns whether the focused pane's cursor is blinking, so the app knows whether it
    /// needs to schedule the next phase change.
    pub fn draw(&mut self, views: &mut [PaneView], dividers: &[Rect], blink_visible: bool) -> bool {
        /// What was read from a pane; it does not change even if the instances must be rebuilt
        struct Prepared<'s> {
            snapshot: Snapshot<'static, 's>,
            colors: Colors,
            cursor: Option<Cursor>,
            rect: Rect,
            dim: f32,
            frame: Option<RgbColor>,
            floating: bool,
        }

        let mut prepared = Vec::with_capacity(views.len());
        let mut blinking = false;
        for view in views.iter_mut() {
            let Ok(snapshot) = view.render_state.update(view.terminal) else {
                continue;
            };
            let Ok(colors) = snapshot.colors() else {
                continue;
            };
            let pane_blinking = snapshot.cursor_blinking().unwrap_or(false);
            if view.focused {
                blinking = pane_blinking;
            }
            // Blink only when focused: unfocused, the cursor is a hollow, steady block
            let hidden_by_blink = view.focused && pane_blinking && !blink_visible;
            let cursor = if hidden_by_blink {
                None
            } else {
                read_cursor(&snapshot, &colors, view.focused)
            };
            // Border: the focus one, and the border of an unfocused floating pane (like dividers)
            let frame = if view.frame && view.leader {
                // ANSI yellow by default: stands out from the normal focus border
                Some(self.panes.leader.unwrap_or(colors.palette[3]))
            } else if view.frame {
                Some(
                    self.panes
                        .focus
                        .or(colors.cursor)
                        .unwrap_or(colors.foreground),
                )
            } else if view.floating {
                Some(
                    self.panes
                        .border
                        .unwrap_or_else(|| mix(colors.foreground, colors.background, 0.75)),
                )
            } else {
                None
            };
            prepared.push(Prepared {
                snapshot,
                colors,
                cursor,
                rect: view.rect,
                dim: if view.dim {
                    self.panes.inactive_dim
                } else {
                    0.0
                },
                frame,
                floating: view.floating,
            });
        }
        let Some((background, foreground)) = prepared
            .first()
            .map(|pane| (pane.colors.background, pane.colors.foreground))
        else {
            return false;
        };
        // By default, dividers use the text color heavily dimmed
        let divider_color = self
            .panes
            .border
            .unwrap_or_else(|| mix(foreground, background, 0.75));
        let window = Rect::new(0, 0, self.config.width, self.config.height);
        let frame_thickness = 2 * self.cache.thickness.max(1);

        // If the atlas fills up mid-frame it is cleared and the instances must be rebuilt
        for _ in 0..2 {
            self.frame.clear();
            // Dividers go beneath everything
            if !dividers.is_empty() {
                let start = self.frame.lengths();
                for rect in dividers {
                    self.frame.backgrounds.push(RectInstance::new(
                        rect.x,
                        rect.y,
                        rect.w,
                        rect.h,
                        divider_color,
                    ));
                }
                let end = self.frame.lengths();
                self.frame.batches.push(Batch {
                    scissor: window,
                    backgrounds: start.0..end.0,
                    glyphs: start.1..end.1,
                    overlays: start.2..end.2,
                });
            }
            for pane in &prepared {
                let start = self.frame.lengths();
                // A floating pane fully covers what is underneath with the theme background
                if pane.floating {
                    let r = pane.rect;
                    self.frame.backgrounds.push(RectInstance::new(
                        r.x,
                        r.y,
                        r.w,
                        r.h,
                        pane.colors.background,
                    ));
                }
                build_frame(
                    &pane.snapshot,
                    &mut self.row_iter,
                    &mut self.cell_iter,
                    &mut self.cache,
                    &mut self.frame,
                    &pane.colors,
                    pane.cursor,
                    self.selection,
                    (pane.rect.x + self.padding, pane.rect.y + self.padding),
                    // Cells are not merged with a floating pane's opaque background
                    start.0 as usize + usize::from(pane.floating),
                    pane.dim,
                );
                if let Some(color) = pane.frame {
                    let (r, t) = (pane.rect, frame_thickness);
                    self.frame.overlays.extend([
                        RectInstance::new(r.x, r.y, r.w, t, color),
                        RectInstance::new(r.x, (r.y + r.h).saturating_sub(t), r.w, t, color),
                        RectInstance::new(r.x, r.y, t, r.h, color),
                        RectInstance::new((r.x + r.w).saturating_sub(t), r.y, t, r.h, color),
                    ]);
                }
                let end = self.frame.lengths();
                self.frame.batches.push(Batch {
                    scissor: pane.rect,
                    backgrounds: start.0..end.0,
                    glyphs: start.1..end.1,
                    overlays: start.2..end.2,
                });
            }
            if !self.cache.take_evicted() {
                break;
            }
        }
        self.present(background);
        blinking
    }

    fn present(&mut self, background: RgbColor) {
        let texture = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(texture)
            | wgpu::CurrentSurfaceTexture::Suboptimal(texture) => texture,
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                self.window.request_redraw();
                return;
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                if let Ok(surface) = self.instance.create_surface(self.window.clone()) {
                    self.surface = surface;
                    self.surface.configure(&self.device, &self.config);
                    self.window.request_redraw();
                }
                return;
            }
            // Hidden window, timeout, or validation error: skip this frame
            _ => return,
        };

        let backgrounds: &[u8] = bytemuck::cast_slice(&self.frame.backgrounds);
        let overlays: &[u8] = bytemuck::cast_slice(&self.frame.overlays);
        self.rect_buffer
            .write(&self.device, &self.queue, &[backgrounds, overlays]);
        self.glyph_buffer.write(
            &self.device,
            &self.queue,
            &[bytemuck::cast_slice(&self.frame.glyphs)],
        );
        let background_bytes = backgrounds.len() as u64;
        let overlay_bytes = overlays.len() as u64;

        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("terminal"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(background.r) / 255.0,
                            g: f64::from(background.g) / 255.0,
                            b: f64::from(background.b) / 255.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });

            // One batch per pane, bottom to top: each clipped to its rectangle, with its
            // backgrounds, text, and decorations in that order
            for batch in &self.frame.batches {
                let Some(clip) = clip_to(batch.scissor, self.config.width, self.config.height)
                else {
                    continue;
                };
                pass.set_scissor_rect(clip.x, clip.y, clip.w, clip.h);
                // 1. Cell backgrounds
                if !batch.backgrounds.is_empty() {
                    pass.set_pipeline(&self.rect_pipeline);
                    pass.set_bind_group(0, &self.rect_bind_group, &[]);
                    pass.set_vertex_buffer(0, self.rect_buffer.buffer.slice(..background_bytes));
                    pass.draw(0..4, batch.backgrounds.clone());
                }
                // 2. Text
                if !batch.glyphs.is_empty() {
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_bind_group(0, &self.glyph_bind_group, &[]);
                    pass.set_vertex_buffer(0, self.glyph_buffer.buffer.slice(..));
                    pass.draw(0..4, batch.glyphs.clone());
                }
                // 3. Underlines, strikethroughs, and cursor on top of the text
                if !batch.overlays.is_empty() {
                    pass.set_pipeline(&self.rect_pipeline);
                    pass.set_bind_group(0, &self.rect_bind_group, &[]);
                    pass.set_vertex_buffer(
                        0,
                        self.rect_buffer
                            .buffer
                            .slice(background_bytes..background_bytes + overlay_bytes),
                    );
                    pass.draw(0..4, batch.overlays.clone());
                }
            }
        }
        self.queue.submit([encoder.finish()]);
        self.window.pre_present_notify();
        self.queue.present(texture);
    }
}

fn padding_for(padding: f64, scale_factor: f64) -> u32 {
    (padding * scale_factor).round() as u32
}

/// Valid clip rectangle within a `width` x `height` surface; `None` if nothing remains
fn clip_to(rect: Rect, width: u32, height: u32) -> Option<Rect> {
    let w = rect.w.min(width.saturating_sub(rect.x));
    let h = rect.h.min(height.saturating_sub(rect.y));
    (w > 0 && h > 0).then_some(Rect::new(rect.x, rect.y, w, h))
}

fn create_pipeline(
    device: &wgpu::Device,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    (vertex, fragment): (&str, &str),
    instance_layout: wgpu::VertexBufferLayout<'_>,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(vertex),
        layout: None,
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some(vertex),
            compilation_options: Default::default(),
            buffers: &[Some(instance_layout)],
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleStrip,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fragment),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

fn rgba(color: RgbColor) -> [u8; 4] {
    [color.r, color.g, color.b, 255]
}

fn mix(a: RgbColor, b: RgbColor, t: f32) -> RgbColor {
    let channel = |a: u8, b: u8| (f32::from(a) * (1.0 - t) + f32::from(b) * t).round() as u8;
    RgbColor {
        r: channel(a.r, b.r),
        g: channel(a.g, b.g),
        b: channel(a.b, b.b),
    }
}

fn resolve_style_color(color: StyleColor, colors: &Colors) -> Option<RgbColor> {
    match color {
        StyleColor::None => None,
        StyleColor::Palette(index) => Some(colors.palette[usize::from(index.0)]),
        StyleColor::Rgb(rgb) => Some(rgb),
    }
}

/// Characters that are not drawn (zero-width joiner, variation selectors, etc.)
fn is_invisible(ch: char) -> bool {
    ch <= ' '
        || ch == '\x7f'
        || matches!(ch, '\u{200B}'..='\u{200F}' | '\u{2060}'..='\u{2064}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}')
}

fn read_cursor(snapshot: &Snapshot<'static, '_>, colors: &Colors, focused: bool) -> Option<Cursor> {
    if !snapshot.cursor_visible().ok()? {
        return None;
    }
    let viewport = snapshot.cursor_viewport().ok()??;
    // When unfocused, the cursor becomes a hollow block
    let style = if focused {
        snapshot.cursor_visual_style().ok()?
    } else {
        CursorVisualStyle::BlockHollow
    };
    let color = snapshot
        .cursor_color()
        .ok()
        .flatten()
        .or(colors.cursor)
        .unwrap_or(colors.foreground);
    Some(Cursor {
        // Over the right half of a wide character, the cursor is drawn from the left half
        col: if viewport.at_wide_tail {
            viewport.x.saturating_sub(1)
        } else {
            viewport.x
        },
        row: viewport.y,
        style,
        color,
    })
}

/// Walks the visible grid and fills `frame` with the instances to draw
#[allow(clippy::too_many_arguments)]
fn build_frame(
    snapshot: &Snapshot<'static, '_>,
    row_iter: &mut RowIterator<'static>,
    cell_iter: &mut CellIterator<'static>,
    cache: &mut GlyphCache,
    frame: &mut Frame,
    colors: &Colors,
    cursor: Option<Cursor>,
    selection: SelectionColors,
    origin: (u32, u32),
    batch_start: usize,
    dim: f32,
) {
    let Ok(mut rows) = row_iter.update(snapshot) else {
        return;
    };
    let (cell_w, cell_h) = (cache.cell_width, cache.cell_height);
    let thickness = cache.thickness;
    let mut row_index = 0u16;

    while let Some(row) = rows.next() {
        let this_row = row_index;
        row_index += 1;
        let y = origin.1 + u32::from(this_row) * cell_h;
        // Selected columns in this row (inclusive); `None` if the selection does not touch it
        let selected_columns = row
            .selection()
            .ok()
            .flatten()
            .map(|range| range.start_x..=range.end_x);
        let Ok(mut cells) = cell_iter.update(row) else {
            continue;
        };

        let mut col = 0u16;
        // A cursor over a wide character also covers the next cell
        let mut cursor_on_tail = false;
        while let Some(cell) = cells.next() {
            let this_col = col;
            col += 1;
            let x = origin.0 + u32::from(this_col) * cell_w;

            let style = cell.style().unwrap_or_default();
            let wide = cell
                .raw_cell()
                .and_then(|raw| raw.wide())
                .unwrap_or(CellWide::Narrow);
            let is_spacer = matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead);
            let span = if wide == CellWide::Wide { 2 } else { 1 };

            let mut on_cursor = cursor_on_tail && wide == CellWide::SpacerTail;
            cursor_on_tail = false;
            if let Some(c) = cursor
                && c.row == this_row
                && c.col == this_col
            {
                on_cursor = true;
                cursor_on_tail = wide == CellWide::Wide;
            }

            let mut fg = cell.fg_color().ok().flatten().unwrap_or(colors.foreground);
            let mut bg = cell.bg_color().ok().flatten().unwrap_or(colors.background);
            if style.inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            if style.faint {
                fg = mix(fg, bg, 0.5);
            }
            // Unfocused pane: the text is dimmed toward the background
            if dim > 0.0 {
                fg = mix(fg, bg, dim);
            }
            // Selected text: custom colors or, by default, the screen colors inverted
            if selected_columns
                .as_ref()
                .is_some_and(|range| range.contains(&this_col))
            {
                bg = selection.background.unwrap_or(colors.foreground);
                fg = selection.foreground.unwrap_or(colors.background);
            }
            if let Some(c) = cursor
                && on_cursor
                && c.style == CursorVisualStyle::Block
            {
                bg = c.color;
                fg = colors.background;
            }

            // Background (adjacent cells of the same color are merged into a single rectangle, but
            // never with one from another pane)
            if bg != colors.background {
                let color = rgba(bg);
                let can_merge = frame.backgrounds.len() > batch_start;
                match frame.backgrounds.last_mut() {
                    Some(last)
                        if can_merge
                            && last.color == color
                            && last.pos[1] == y as f32
                            && last.pos[0] + last.size[0] == x as f32 =>
                    {
                        last.size[0] += cell_w as f32;
                    }
                    _ => frame
                        .backgrounds
                        .push(RectInstance::new(x, y, cell_w, cell_h, bg)),
                }
            }

            // Text
            if !style.invisible && !is_spacer && cell.graphemes_len().unwrap_or(0) > 0 {
                frame.text.clear();
                if cell.graphemes_utf8(&mut frame.text).is_ok() {
                    for ch in frame.text.chars().filter(|&ch| !is_invisible(ch)) {
                        let key = GlyphKey {
                            ch,
                            bold: style.bold,
                            italic: style.italic,
                        };
                        if let Some(glyph) = cache.glyph(key) {
                            frame.glyphs.push(GlyphInstance {
                                pos: [(x as i32 + glyph.x) as f32, (y as i32 + glyph.y) as f32],
                                size: [glyph.width as f32, glyph.height as f32],
                                atlas: [glyph.atlas_x as f32, glyph.atlas_y as f32],
                                color: rgba(fg),
                            });
                        }
                    }
                }
            }

            // Decorations. "Spacer" cells draw nothing: the wide cell already covers both.
            if !is_spacer {
                let width = cell_w * span;
                let decoration = resolve_style_color(style.underline_color, colors).unwrap_or(fg);
                match style.underline {
                    Underline::None => {}
                    Underline::Double => {
                        frame.overlays.push(RectInstance::new(
                            x,
                            y + cache.underline_y,
                            width,
                            thickness,
                            decoration,
                        ));
                        frame.overlays.push(RectInstance::new(
                            x,
                            y + cache.underline_y.saturating_sub(2 * thickness),
                            width,
                            thickness,
                            decoration,
                        ));
                    }
                    // Wavy, dotted, and dashed are approximated with a single line
                    _ => frame.overlays.push(RectInstance::new(
                        x,
                        y + cache.underline_y,
                        width,
                        thickness,
                        decoration,
                    )),
                }
                if style.strikethrough {
                    frame.overlays.push(RectInstance::new(
                        x,
                        y + cache.strikethrough_y,
                        width,
                        thickness,
                        fg,
                    ));
                }
                if style.overline {
                    frame
                        .overlays
                        .push(RectInstance::new(x, y, width, thickness, fg));
                }

                // The block cursor was already painted as a background; the rest are rectangles
                // on top
                if let Some(c) = cursor
                    && on_cursor
                {
                    let color = c.color;
                    match c.style {
                        CursorVisualStyle::Bar => frame.overlays.push(RectInstance::new(
                            x,
                            y,
                            thickness * 2,
                            cell_h,
                            color,
                        )),
                        CursorVisualStyle::Underline => frame.overlays.push(RectInstance::new(
                            x,
                            y + cell_h.saturating_sub(thickness * 2),
                            width,
                            thickness * 2,
                            color,
                        )),
                        CursorVisualStyle::BlockHollow => {
                            let t = thickness;
                            frame.overlays.extend([
                                RectInstance::new(x, y, width, t, color),
                                RectInstance::new(x, y + cell_h - t, width, t, color),
                                RectInstance::new(x, y, t, cell_h, color),
                                RectInstance::new(x + width - t, y, t, cell_h, color),
                            ]);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
