//! Dibujo de la cuadrícula del terminal con wgpu: un quad instanciado por cada rectángulo
//! (fondos, subrayados, cursor) y por cada glifo, todo en un único render pass.

use std::mem::size_of;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use libghostty_vt::render::{CellIterator, Colors, CursorVisualStyle, RowIterator, Snapshot};
use libghostty_vt::screen::CellWide;
use libghostty_vt::style::{RgbColor, StyleColor, Underline};
use libghostty_vt::{RenderState, Terminal};
use winit::window::Window;

use crate::config::Config;
use crate::glyphs::{GlyphCache, GlyphKey};

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

/// Instancias de un frame, en el orden en que se dibujan: fondos, glifos y encima las
/// decoraciones. Los vectores se reutilizan entre frames para no reservar memoria cada vez.
#[derive(Default)]
struct Frame {
    backgrounds: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    overlays: Vec<RectInstance>,
    text: String,
}

impl Frame {
    fn clear(&mut self) {
        self.backgrounds.clear();
        self.glyphs.clear();
        self.overlays.clear();
    }
}

#[derive(Clone, Copy)]
struct Cursor {
    col: u16,
    row: u16,
    style: CursorVisualStyle,
    color: RgbColor,
}

/// Buffer de vértices que crece (potencias de dos) cuando el frame necesita más espacio
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

    /// Escribe los fragmentos uno a continuación del otro
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
    /// Margen en pixeles lógicos (de la configuración) y en pixeles físicos con la escala actual
    padding_logical: f64,
    padding: u32,
    frame: Frame,
    // Estado de libghostty que solo se usa para dibujar
    render_state: RenderState<'static>,
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
            .ok_or("la superficie no es compatible con el adaptador")?;
        // Los colores del terminal ya están en sRGB: se escriben tal cual, sin conversión
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
        let rect_attributes = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Unorm8x4];
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

        // Los layouts se deducen del shader: el de rectángulos solo ve `globals`
        let rect_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rectángulos"),
            layout: &rect_pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let atlas = cache.atlas_view();
        let glyph_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glifos"),
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
            rect_buffer: GrowBuffer::new(&device, "rectángulos"),
            glyph_buffer: GrowBuffer::new(&device, "glifos"),
            padding_logical: settings.padding,
            padding: padding_for(settings.padding, scale_factor),
            frame: Frame::default(),
            render_state: RenderState::new().map_err(|e| e.to_string())?,
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

    /// Tamaño de una celda en pixeles físicos
    pub fn cell_size(&self) -> (u32, u32) {
        (self.cache.cell_width, self.cache.cell_height)
    }

    /// Margen alrededor de la cuadrícula en pixeles físicos
    pub fn padding(&self) -> u32 {
        self.padding
    }

    /// Columnas y filas que caben en una ventana de ese tamaño (en pixeles físicos)
    pub fn grid_size(&self, width: u32, height: u32) -> (u16, u16) {
        let cols = width.saturating_sub(2 * self.padding) / self.cache.cell_width;
        let rows = height.saturating_sub(2 * self.padding) / self.cache.cell_height;
        (cols.clamp(1, u16::MAX.into()) as u16, rows.clamp(1, u16::MAX.into()) as u16)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        // wgpu no admite superficies de 0×0 (la ventana minimizada reporta ese tamaño)
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

    /// Aplica la fuente y el margen de una configuración nueva. Si la fuente no se puede cargar
    /// devuelve el error sin cambiar nada.
    pub fn reconfigure(&mut self, settings: &Config) -> Result<(), String> {
        self.cache.set_font(&settings.font)?;
        self.padding_logical = settings.padding;
        self.padding = padding_for(settings.padding, self.window.scale_factor());
        Ok(())
    }

    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.cache.set_scale_factor(scale_factor);
        self.padding = padding_for(self.padding_logical, scale_factor);
    }

    pub fn draw(&mut self, terminal: &Terminal<'static, 'static>, focused: bool) {
        let Ok(snapshot) = self.render_state.update(terminal) else {
            return;
        };
        let Ok(colors) = snapshot.colors() else {
            return;
        };
        let cursor = read_cursor(&snapshot, &colors, focused);

        // Si el atlas se llena a mitad de frame se vacía y hay que rehacer las instancias
        for _ in 0..2 {
            self.frame.clear();
            build_frame(
                &snapshot,
                &mut self.row_iter,
                &mut self.cell_iter,
                &mut self.cache,
                &mut self.frame,
                &colors,
                cursor,
                self.padding,
            );
            if !self.cache.take_evicted() {
                break;
            }
        }
        self.present(colors.background);
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
            // Ventana oculta, timeout o error de validación: se salta este frame
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

            // 1. Fondos de celda
            pass.set_pipeline(&self.rect_pipeline);
            pass.set_bind_group(0, &self.rect_bind_group, &[]);
            if !self.frame.backgrounds.is_empty() {
                pass.set_vertex_buffer(0, self.rect_buffer.buffer.slice(..background_bytes));
                pass.draw(0..4, 0..self.frame.backgrounds.len() as u32);
            }
            // 2. Texto
            if !self.frame.glyphs.is_empty() {
                pass.set_pipeline(&self.glyph_pipeline);
                pass.set_bind_group(0, &self.glyph_bind_group, &[]);
                pass.set_vertex_buffer(0, self.glyph_buffer.buffer.slice(..));
                pass.draw(0..4, 0..self.frame.glyphs.len() as u32);
            }
            // 3. Subrayados, tachados y cursor por encima del texto
            if !self.frame.overlays.is_empty() {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_bind_group(0, &self.rect_bind_group, &[]);
                pass.set_vertex_buffer(
                    0,
                    self.rect_buffer
                        .buffer
                        .slice(background_bytes..background_bytes + overlay_bytes),
                );
                pass.draw(0..4, 0..self.frame.overlays.len() as u32);
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

/// Caracteres que no se dibujan (unión de ancho cero, selectores de variación, etc.)
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
    // Sin foco, el cursor pasa a ser un bloque hueco
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
        // Sobre la mitad derecha de un carácter ancho, el cursor se dibuja desde la izquierda
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

/// Recorre la cuadrícula visible y llena `frame` con las instancias a dibujar
#[allow(clippy::too_many_arguments)]
fn build_frame(
    snapshot: &Snapshot<'static, '_>,
    row_iter: &mut RowIterator<'static>,
    cell_iter: &mut CellIterator<'static>,
    cache: &mut GlyphCache,
    frame: &mut Frame,
    colors: &Colors,
    cursor: Option<Cursor>,
    padding: u32,
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
        let y = padding + u32::from(this_row) * cell_h;
        let Ok(mut cells) = cell_iter.update(row) else {
            continue;
        };

        let mut col = 0u16;
        // El cursor sobre un carácter ancho cubre también la celda siguiente
        let mut cursor_on_tail = false;
        while let Some(cell) = cells.next() {
            let this_col = col;
            col += 1;
            let x = padding + u32::from(this_col) * cell_w;

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
            if let Some(c) = cursor
                && on_cursor
                && c.style == CursorVisualStyle::Block
            {
                bg = c.color;
                fg = colors.background;
            }

            // Fondo (las celdas contiguas del mismo color se funden en un solo rectángulo)
            if bg != colors.background {
                let color = rgba(bg);
                match frame.backgrounds.last_mut() {
                    Some(last)
                        if last.color == color
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

            // Texto
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

            // Decoraciones. Las celdas "spacer" no dibujan nada: la celda ancha ya cubre ambas.
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
                    // Ondulado, punteado y discontinuo se aproximan con una línea simple
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

                // El cursor en bloque ya se pintó como fondo; el resto son rectángulos encima
                if let Some(c) = cursor
                    && on_cursor
                {
                    let color = c.color;
                    match c.style {
                        CursorVisualStyle::Bar => frame
                            .overlays
                            .push(RectInstance::new(x, y, thickness * 2, cell_h, color)),
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
