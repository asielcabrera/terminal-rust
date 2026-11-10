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

/// Fuente de reserva. Las de CJK pesan decenas de MB y fontdue procesa todos sus glifos al
/// cargarlas, así que primero se mira si la fuente tiene el carácter (leyendo solo su tabla `cmap`,
/// sobre el archivo mapeado en memoria) y solo se carga entera la primera vez que hace falta de
/// verdad.
struct LazyFont {
    family: String,
    /// Es una Nerd Font: para los iconos (área de uso privado) tiene prioridad sobre las demás
    nerd: bool,
    face: Option<Option<ID>>,
    font: Option<Option<Font>>,
}

impl LazyFont {
    fn new(family: String) -> Self {
        Self {
            nerd: family.contains("Nerd Font"),
            family,
            face: None,
            font: None,
        }
    }

    fn face(&mut self, db: &Database) -> Option<ID> {
        *self
            .face
            .get_or_insert_with(|| find_face(db, &self.family, false, false))
    }

    /// ¿Tiene esta fuente un glifo para `ch`? No carga la fuente.
    fn covers(&mut self, db: &Database, ch: char) -> bool {
        let Some(id) = self.face(db) else {
            return false;
        };
        db.with_face_data(id, |data, index| {
            ttf_parser::Face::parse(data, index)
                .ok()
                .and_then(|face| face.glyph_index(ch))
                .is_some()
        })
        .unwrap_or(false)
    }

    fn get(&mut self, db: &Database, px: f32) -> Option<&Font> {
        let id = self.face(db);
        self.font
            .get_or_insert_with(|| id.and_then(|id| load_face(db, id, px)))
            .as_ref()
    }
}

/// Una Nerd Font instalada para usar como último recurso con los iconos (Powerline, Font Awesome,
/// Devicons...) cuando ni la fuente principal ni las de la configuración los tienen. Prefiere las
/// de solo símbolos y luego las variantes `Mono`; a igualdad, el orden alfabético para que la
/// elección sea siempre la misma.
fn detect_nerd_font(db: &Database) -> Option<String> {
    let mut best: Option<(u8, String)> = None;
    for face in db.faces() {
        for (name, _language) in &face.families {
            if !name.contains("Nerd Font") {
                continue;
            }
            let rank = if name.starts_with("Symbols") {
                0
            } else if name.ends_with("Nerd Font Mono") {
                1
            } else if face.monospaced {
                2
            } else {
                3
            };
            let candidate = (rank, name.clone());
            if best.as_ref().is_none_or(|current| candidate < *current) {
                best = Some(candidate);
            }
        }
    }
    best.map(|(_, name)| name)
}

/// Una variante de la fuente principal (negrita, cursiva...) que se carga cuando hace falta. Una
/// Nerd Font pesa unos 2,5 MB con miles de glifos y fontdue los procesa todos al cargarla: con las
/// cuatro variantes eso eran varios segundos de arranque.
struct StyledFace {
    id: Option<ID>,
    font: Option<Option<Font>>,
}

impl StyledFace {
    fn new(db: &Database, family: &str, bold: bool, italic: bool) -> Self {
        Self {
            id: find_face(db, family, bold, italic),
            font: None,
        }
    }

    fn get(&mut self, db: &Database, px: f32) -> Option<&Font> {
        let id = self.id;
        self.font
            .get_or_insert_with(|| id.and_then(|id| load_face(db, id, px)))
            .as_ref()
    }
}

/// Área de uso privado (BMP y suplementaria A), donde las Nerd Fonts ponen sus iconos
fn is_private_use(ch: char) -> bool {
    matches!(ch, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}')
}

struct Fonts {
    regular: Font,
    bold: StyledFace,
    italic: StyledFace,
    bold_italic: StyledFace,
    fallbacks: Vec<LazyFont>,
    px: f32,
}

impl Fonts {
    fn load(db: &Database, config: &FontConfig, scale_factor: f64) -> Result<Self, String> {
        let px = config.size * scale_factor as f32;
        let regular = find_face(db, &config.family, false, false)
            .and_then(|id| load_face(db, id, px))
            .ok_or_else(|| format!("no se encontró la fuente \"{}\" en el sistema", config.family))?;

        let mut fallbacks: Vec<LazyFont> = config.fallback.iter().cloned().map(LazyFont::new).collect();
        // Última red: cualquier Nerd Font instalada, solo para lo que nadie más tiene
        if let Some(nerd) = detect_nerd_font(db)
            && nerd != config.family
            && !config.fallback.contains(&nerd)
        {
            fallbacks.push(LazyFont::new(nerd));
        }

        Ok(Self {
            regular,
            bold: StyledFace::new(db, &config.family, true, false),
            italic: StyledFace::new(db, &config.family, false, true),
            bold_italic: StyledFace::new(db, &config.family, true, true),
            fallbacks,
            px,
        })
    }

    fn rasterize(&mut self, db: &Database, key: GlyphKey) -> (Metrics, Vec<u8>) {
        let px = self.px;
        let styled = match (key.bold, key.italic) {
            (false, false) => None,
            (true, false) => self.bold.get(db, px),
            (false, true) => self.italic.get(db, px),
            (true, true) => self.bold_italic.get(db, px),
        };
        if let Some(font) = styled
            && font.has_glyph(key.ch)
        {
            return font.rasterize(key.ch, px);
        }
        if self.regular.has_glyph(key.ch) {
            return self.regular.rasterize(key.ch, px);
        }

        // Las de reserva, en orden. En el área de uso privado las fuentes de CJK y de símbolos del
        // sistema traen glifos antiguos que no tienen nada que ver con los iconos de Nerd Fonts,
        // así que ahí las Nerd Fonts van primero. `covers` es barato y evita cargar una fuente
        // enorme solo para descubrir que no tiene el carácter.
        let private_use = is_private_use(key.ch);
        for nerd_pass in [true, false] {
            if nerd_pass && !private_use {
                continue;
            }
            for slot in &mut self.fallbacks {
                if nerd_pass && !slot.nerd {
                    continue;
                }
                if slot.covers(db, key.ch)
                    && let Some(font) = slot.get(db, px)
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


#[cfg(test)]
mod tests {
    use super::*;

    // Powerline (separador) e icono de casa de Font Awesome, ambos en el área de uso privado
    const POWERLINE: char = '\u{E0B0}';
    const HOME_ICON: char = '\u{F015}';
    // Material Design de Nerd Fonts v3, en el área de uso privado suplementaria
    const MATERIAL_ICON: char = '\u{F0001}';

    fn database() -> Database {
        let mut db = Database::new();
        db.load_system_fonts();
        db
    }

    fn has_family(db: &Database, name: &str) -> bool {
        find_face(db, name, false, false).is_some()
    }

    fn config(family: &str) -> FontConfig {
        FontConfig {
            family: family.to_string(),
            ..FontConfig::default()
        }
    }

    fn plain(ch: char) -> GlyphKey {
        GlyphKey {
            ch,
            bold: false,
            italic: false,
        }
    }

    /// El glifo se dibujó de verdad: tiene bitmap y no es el cuadrito .notdef de la fuente principal
    fn assert_drawn(fonts: &mut Fonts, db: &Database, ch: char) {
        let (metrics, bitmap) = fonts.rasterize(db, plain(ch));
        let (notdef_metrics, notdef_bitmap) = fonts.regular.rasterize_indexed(0, fonts.px);
        assert!(
            metrics.width > 0 && bitmap.iter().any(|&b| b > 0),
            "{ch:?} salió vacío"
        );
        let is_notdef = metrics.width == notdef_metrics.width
            && metrics.height == notdef_metrics.height
            && bitmap == notdef_bitmap;
        assert!(!is_notdef, "{ch:?} salió como .notdef");
    }

    #[test]
    fn a_nerd_font_as_the_primary_font_draws_icons() {
        let db = database();
        let family = "JetBrainsMono Nerd Font Mono";
        if !has_family(&db, family) {
            eprintln!("saltada: {family} no está instalada");
            return;
        }
        let mut fonts = Fonts::load(&db, &config(family), 1.0).unwrap();
        for ch in [POWERLINE, HOME_ICON, MATERIAL_ICON] {
            assert_drawn(&mut fonts, &db, ch);
        }
    }

    #[test]
    fn icons_are_found_in_an_installed_nerd_font_when_the_primary_font_lacks_them() {
        // Consolas no trae iconos; el último recurso es cualquier Nerd Font instalada
        let db = database();
        if !has_family(&db, "Consolas") || detect_nerd_font(&db).is_none() {
            eprintln!("saltada: hace falta Consolas y alguna Nerd Font");
            return;
        }
        let mut fonts = Fonts::load(&db, &config("Consolas"), 1.0).unwrap();
        for ch in [POWERLINE, HOME_ICON, MATERIAL_ICON] {
            assert_drawn(&mut fonts, &db, ch);
        }
    }

    #[test]
    fn a_configured_fallback_is_used_for_private_use_icons_too() {
        let db = database();
        let nerd = "JetBrainsMono Nerd Font Mono";
        if !has_family(&db, "Consolas") || !has_family(&db, nerd) {
            eprintln!("saltada: hace falta Consolas y {nerd}");
            return;
        }
        // Con la Nerd Font en la lista de reserva explícita (antes se saltaban para estos códigos)
        let settings = FontConfig {
            family: "Consolas".to_string(),
            fallback: vec![nerd.to_string()],
            ..FontConfig::default()
        };
        let mut fonts = Fonts::load(&db, &settings, 1.0).unwrap();
        assert_drawn(&mut fonts, &db, POWERLINE);
    }

    #[test]
    fn coverage_is_checked_without_loading_the_font() {
        let db = database();
        if !has_family(&db, "Microsoft YaHei") {
            eprintln!("saltada: Microsoft YaHei no está instalada");
            return;
        }
        let mut slot = LazyFont::new("Microsoft YaHei".to_string());
        assert!(slot.covers(&db, '日'));
        assert!(!slot.covers(&db, POWERLINE));
        assert!(slot.font.is_none(), "covers no debe cargar la fuente (pesa decenas de MB)");
        assert!(slot.get(&db, 16.0).is_some());
        assert!(slot.font.is_some());
    }

    #[test]
    fn nerd_fonts_win_over_system_fonts_for_private_use_icons() {
        // Malgun Gothic, YaHei y Segoe UI Symbol traen glifos antiguos en el área de uso privado:
        // aunque estén en la lista de reserva, un icono tiene que salir de la Nerd Font
        let db = database();
        let Some(nerd) = detect_nerd_font(&db) else {
            eprintln!("saltada: no hay ninguna Nerd Font instalada");
            return;
        };
        if !has_family(&db, "Consolas") || !has_family(&db, "Malgun Gothic") {
            eprintln!("saltada: hace falta Consolas y Malgun Gothic");
            return;
        }
        let with_system_fonts_first = FontConfig {
            family: "Consolas".to_string(),
            fallback: vec!["Malgun Gothic".to_string(), "Microsoft YaHei".to_string()],
            ..FontConfig::default()
        };
        let mut fonts = Fonts::load(&db, &with_system_fonts_first, 1.0).unwrap();
        let mut reference = Fonts::load(&db, &config(&nerd), 1.0).unwrap();
        for ch in [POWERLINE, HOME_ICON, MATERIAL_ICON] {
            let (metrics, bitmap) = fonts.rasterize(&db, plain(ch));
            let (expected_metrics, expected) = reference.rasterize(&db, plain(ch));
            assert_eq!((metrics.width, metrics.height), (expected_metrics.width, expected_metrics.height), "{ch:?}");
            assert_eq!(bitmap, expected, "{ch:?} no salió de {nerd}");
        }
    }

    #[test]
    fn bold_and_italic_faces_are_loaded_only_when_needed() {
        let db = database();
        if !has_family(&db, "Consolas") {
            eprintln!("saltada: Consolas no está instalada");
            return;
        }
        let mut fonts = Fonts::load(&db, &config("Consolas"), 1.0).unwrap();
        assert!(fonts.bold.font.is_none() && fonts.italic.font.is_none() && fonts.bold_italic.font.is_none());

        fonts.rasterize(&db, plain('a'));
        assert!(fonts.bold.font.is_none(), "una letra normal no debe cargar la negrita");

        fonts.rasterize(&db, GlyphKey { ch: 'a', bold: true, italic: false });
        assert!(fonts.bold.font.is_some(), "la negrita se carga al usarla");
        assert!(fonts.italic.font.is_none() && fonts.bold_italic.font.is_none());
    }

    #[test]
    fn private_use_ranges_are_recognized() {
        assert!(is_private_use('\u{E0B0}') && is_private_use('\u{F8FF}') && is_private_use('\u{F0001}'));
        assert!(!is_private_use('a') && !is_private_use('日') && !is_private_use('\u{1F600}'));
    }

    #[test]
    fn a_missing_family_covers_nothing() {
        let db = database();
        let mut slot = LazyFont::new("Fuente Que No Existe".to_string());
        assert!(!slot.covers(&db, 'a'));
        assert!(slot.get(&db, 16.0).is_none());
    }

    #[test]
    fn the_automatic_nerd_font_is_deterministic_and_prefers_mono_variants() {
        let db = database();
        let Some(chosen) = detect_nerd_font(&db) else {
            eprintln!("saltada: no hay ninguna Nerd Font instalada");
            return;
        };
        assert_eq!(detect_nerd_font(&db), Some(chosen.clone()), "debe elegir siempre la misma");
        assert!(chosen.contains("Nerd Font"), "{chosen}");
        let any_symbols = db
            .faces()
            .any(|f| f.families.iter().any(|(n, _)| n.starts_with("Symbols") && n.contains("Nerd Font")));
        let any_mono = db
            .faces()
            .any(|f| f.families.iter().any(|(n, _)| n.ends_with("Nerd Font Mono")));
        if any_symbols {
            assert!(chosen.starts_with("Symbols"), "{chosen}");
        } else if any_mono {
            assert!(chosen.ends_with("Nerd Font Mono"), "{chosen}");
        }
    }
}
