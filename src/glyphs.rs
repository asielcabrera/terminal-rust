//! Fuentes, rasterizado y atlas de glifos en la GPU.

use fontdb::{Database, Family, ID, Query, Stretch, Style, Weight};
use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;

use crate::config::FontConfig;

/// Lado (cuadrado) del atlas en texels
const ATLAS_SIZE: u32 = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub ch: char,
    pub bold: bool,
    pub italic: bool,
}

/// Dónde está el bitmap de un glifo respecto a su celda y dentro del atlas
#[derive(Clone, Copy)]
pub struct Glyph {
    /// Desplazamiento respecto a la esquina superior izquierda de la celda
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub atlas_x: u32,
    pub atlas_y: u32,
}

/// Busca la cara de `family` con ese peso y estilo. Si se pide negrita o cursiva y la familia no
/// la tiene, la base de datos devolvería "lo más parecido" (la regular): en ese caso no se usa,
/// para que `rasterize` caiga a la regular en vez de hacerse pasar por negrita.
fn find_face(db: &Database, family: &str, bold: bool, italic: bool) -> Option<ID> {
    let query = Query {
        families: &[Family::Name(family)],
        weight: if bold { Weight::BOLD } else { Weight::NORMAL },
        stretch: Stretch::Normal,
        style: if italic { Style::Italic } else { Style::Normal },
    };
    let id = db.query(&query)?;
    if !bold && !italic {
        return Some(id);
    }
    let face = db.face(id)?;
    let is_bold = face.weight.0 >= 600;
    let is_italic = face.style != Style::Normal;
    (is_bold == bold && is_italic == italic).then_some(id)
}

fn load_face(db: &Database, id: ID, px: f32) -> Option<Font> {
    db.with_face_data(id, |data, collection_index| {
        let settings = FontSettings {
            collection_index,
            scale: px,
            load_substitutions: false,
        };
        Font::from_bytes(data, settings).ok()
    })?
}

/// Fuente de reserva que se carga la primera vez que hace falta: las de CJK pesan decenas de MB
/// y fontdue procesa todos sus glifos al cargarlas.
struct LazyFont {
    family: String,
    font: Option<Option<Font>>,
}

impl LazyFont {
    fn get(&mut self, db: &Database, px: f32) -> Option<&Font> {
        self.font
            .get_or_insert_with(|| {
                find_face(db, &self.family, false, false).and_then(|id| load_face(db, id, px))
            })
            .as_ref()
    }
}

struct Fonts {
    regular: Font,
    bold: Option<Font>,
    italic: Option<Font>,
    bold_italic: Option<Font>,
    fallbacks: Vec<LazyFont>,
    px: f32,
}

impl Fonts {
    fn load(db: &Database, config: &FontConfig, scale_factor: f64) -> Result<Self, String> {
        let px = config.size * scale_factor as f32;
        let face = |bold, italic| {
            find_face(db, &config.family, bold, italic).and_then(|id| load_face(db, id, px))
        };
        let regular = face(false, false)
            .ok_or_else(|| format!("no se encontró la fuente \"{}\" en el sistema", config.family))?;
        Ok(Self {
            regular,
            bold: face(true, false),
            italic: face(false, true),
            bold_italic: face(true, true),
            fallbacks: config
                .fallback
                .iter()
                .map(|family| LazyFont {
                    family: family.clone(),
                    font: None,
                })
                .collect(),
            px,
        })
    }

    fn rasterize(&mut self, db: &Database, key: GlyphKey) -> (Metrics, Vec<u8>) {
        let px = self.px;
        let styled = match (key.bold, key.italic) {
            (false, false) => None,
            (true, false) => self.bold.as_ref(),
            (false, true) => self.italic.as_ref(),
            (true, true) => self.bold_italic.as_ref(),
        };
        if let Some(font) = styled
            && font.has_glyph(key.ch)
        {
            return font.rasterize(key.ch, px);
        }
        if self.regular.has_glyph(key.ch) {
            return self.regular.rasterize(key.ch, px);
        }
        // Powerline y Nerd Fonts viven en el área de uso privado y no están en las fuentes del
        // sistema: no vale la pena cargar las de reserva para no encontrarlas
        if !('\u{E000}'..='\u{F8FF}').contains(&key.ch) {
            for slot in &mut self.fallbacks {
                if let Some(font) = slot.get(db, px)
                    && font.has_glyph(key.ch)
                {
                    return font.rasterize(key.ch, px);
                }
            }
        }
        // Glifo 0 = .notdef (el cuadrito de "carácter no disponible")
        self.regular.rasterize_indexed(0, px)
    }
}

pub struct GlyphCache {
    /// Fuentes instaladas en el sistema, escaneadas una sola vez
    db: Database,
    font: FontConfig,
    scale_factor: f64,
    fonts: Fonts,
    pub cell_width: u32,
    pub cell_height: u32,
    /// Grosor de subrayados, tachado y cursor
    pub thickness: u32,
    /// Distancias desde el borde superior de la celda
    pub underline_y: u32,
    pub strikethrough_y: u32,
    baseline: u32,
    texture: wgpu::Texture,
    queue: wgpu::Queue,
    glyphs: HashMap<GlyphKey, Option<Glyph>>,
    // Empaquetado por filas: siguiente posición libre y alto de la fila actual
    next_x: u32,
    next_y: u32,
    row_height: u32,
    evicted: bool,
}

impl GlyphCache {
    pub fn new(
        device: &wgpu::Device,
        queue: wgpu::Queue,
        scale_factor: f64,
        font: &FontConfig,
    ) -> Result<Self, String> {
        let mut db = Database::new();
        db.load_system_fonts();
        let fonts = Fonts::load(&db, font, scale_factor)?;

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas de glifos"),
            size: wgpu::Extent3d {
                width: ATLAS_SIZE,
                height: ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let mut cache = Self {
            db,
            font: font.clone(),
            scale_factor,
            fonts,
            cell_width: 0,
            cell_height: 0,
            thickness: 1,
            underline_y: 0,
            strikethrough_y: 0,
            baseline: 0,
            texture,
            queue,
            glyphs: HashMap::new(),
            next_x: 0,
            next_y: 0,
            row_height: 0,
            evicted: false,
        };
        cache.measure();
        Ok(cache)
    }

    pub fn atlas_view(&self) -> wgpu::TextureView {
        self.texture.create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// Recarga las fuentes al nuevo tamaño en pixeles y vacía el atlas
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        if let Ok(fonts) = Fonts::load(&self.db, &self.font, scale_factor) {
            self.scale_factor = scale_factor;
            self.install(fonts);
        }
    }

    /// Cambia de fuente (al recargar la configuración). Es atómico: si la familia no existe,
    /// devuelve el error y se conserva la fuente anterior. Si no cambió nada, no hace nada.
    pub fn set_font(&mut self, font: &FontConfig) -> Result<(), String> {
        if *font == self.font {
            return Ok(());
        }
        let fonts = Fonts::load(&self.db, font, self.scale_factor)?;
        self.font = font.clone();
        self.install(fonts);
        Ok(())
    }

    fn install(&mut self, fonts: Fonts) {
        self.fonts = fonts;
        self.measure();
        self.reset();
    }

    /// Indica (una sola vez) si el atlas se llenó y se vació durante el frame en curso. Las
    /// instancias armadas antes del vaciado apuntan a posiciones obsoletas y hay que rehacerlas.
    pub fn take_evicted(&mut self) -> bool {
        std::mem::take(&mut self.evicted)
    }

    pub fn glyph(&mut self, key: GlyphKey) -> Option<Glyph> {
        if let Some(glyph) = self.glyphs.get(&key) {
            return *glyph;
        }
        let glyph = self.render(key);
        self.glyphs.insert(key, glyph);
        glyph
    }

    fn measure(&mut self) {
        let px = self.fonts.px;
        let font = &self.fonts.regular;
        let (ascent, descent, gap) = font
            .horizontal_line_metrics(px)
            .map_or((px * 0.8, -px * 0.2, 0.0), |m| (m.ascent, m.descent, m.line_gap));
        self.cell_width = font.metrics('0', px).advance_width.round().max(1.0) as u32;
        self.cell_height = (ascent - descent + gap).ceil().max(1.0) as u32;
        self.baseline = (ascent + gap / 2.0).round().max(0.0) as u32;
        self.thickness = (px / 14.0).round().max(1.0) as u32;
        self.underline_y = (self.baseline + (self.cell_height - self.baseline.min(self.cell_height)) / 3)
            .min(self.cell_height.saturating_sub(self.thickness));
        self.strikethrough_y = self.baseline.saturating_sub((px * 0.3).round() as u32);
    }

    fn reset(&mut self) {
        self.glyphs.clear();
        self.next_x = 0;
        self.next_y = 0;
        self.row_height = 0;
    }

    fn render(&mut self, key: GlyphKey) -> Option<Glyph> {
        let (metrics, bitmap) = self.fonts.rasterize(&self.db, key);
        if metrics.width == 0 || metrics.height == 0 {
            return None; // espacios y similares: no hay nada que dibujar
        }
        let (width, height) = (metrics.width as u32, metrics.height as u32);
        let (atlas_x, atlas_y) = self.allocate(width, height)?;

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: atlas_x,
                    y: atlas_y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            &bitmap,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        Some(Glyph {
            x: metrics.xmin,
            // ymin es el borde inferior del bitmap sobre la línea base (positivo hacia arriba)
            y: self.baseline as i32 - (metrics.ymin + metrics.height as i32),
            width,
            height,
            atlas_x,
            atlas_y,
        })
    }

    fn allocate(&mut self, width: u32, height: u32) -> Option<(u32, u32)> {
        if width > ATLAS_SIZE || height > ATLAS_SIZE {
            return None;
        }
        if self.next_x + width > ATLAS_SIZE {
            self.next_x = 0;
            self.next_y += self.row_height;
            self.row_height = 0;
        }
        if self.next_y + height > ATLAS_SIZE {
            self.reset();
            self.evicted = true;
        }
        let position = (self.next_x, self.next_y);
        self.next_x += width;
        self.row_height = self.row_height.max(height);
        Some(position)
    }
}
