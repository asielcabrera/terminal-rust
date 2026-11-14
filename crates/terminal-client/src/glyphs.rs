//! Fonts, rasterization, and the GPU glyph atlas.

use fontdb::{Database, Family, ID, Query, Stretch, Style, Weight};
use fontdue::{Font, FontSettings, Metrics};
use std::collections::HashMap;

use crate::config::FontConfig;

/// Side length of the (square) atlas, in texels
const ATLAS_SIZE: u32 = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub ch: char,
    pub bold: bool,
    pub italic: bool,
}

/// Where a glyph's bitmap sits relative to its cell and inside the atlas
#[derive(Clone, Copy)]
pub struct Glyph {
    /// Offset from the top-left corner of the cell
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub atlas_x: u32,
    pub atlas_y: u32,
}

/// Finds the face of `family` with that weight and style. If bold or italic is requested and the
/// family lacks it, the database would return the "closest match" (the regular): in that case it
/// is not used, so that `rasterize` falls back to the regular instead of passing it off as bold.
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

/// Glyph of `ch` according to the font's Unicode cmap. fontdue merges every cmap subtable into a
/// single map, so in fonts that also ship a Mac Roman table (Menlo) U+0080..U+00FF end up pointing
/// at the Mac Roman glyph ('ó' drawn as 'Û'). ttf-parser only reads the Unicode subtables, and it
/// works on the memory-mapped file without loading the font.
fn unicode_glyph(db: &Database, id: ID, ch: char) -> Option<u16> {
    db.with_face_data(id, |data, index| {
        ttf_parser::Face::parse(data, index).ok()?.glyph_index(ch)
    })
    .flatten()
    .map(|glyph| glyph.0)
    .filter(|&glyph| glyph != 0)
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

/// Fallback font. CJK fonts weigh tens of MB and fontdue processes all their glyphs on load, so
/// we first check whether the font has the character (reading only its `cmap` table from the
/// memory-mapped file) and only load it fully the first time it is actually needed.
struct LazyFont {
    family: String,
    /// It is a Nerd Font: for icons (Private Use Area) it takes priority over the others
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

    /// Glyph of `ch` in this font, if it has one. Does not load the font.
    fn glyph(&mut self, db: &Database, ch: char) -> Option<u16> {
        unicode_glyph(db, self.face(db)?, ch)
    }

    /// Does this font have a glyph for `ch`? Does not load the font.
    #[cfg(test)]
    fn covers(&mut self, db: &Database, ch: char) -> bool {
        self.glyph(db, ch).is_some()
    }

    fn get(&mut self, db: &Database, px: f32) -> Option<&Font> {
        let id = self.face(db);
        self.font
            .get_or_insert_with(|| id.and_then(|id| load_face(db, id, px)))
            .as_ref()
    }
}

/// An installed Nerd Font used as a last resort for icons (Powerline, Font Awesome, Devicons...)
/// when neither the primary font nor the configured ones have them. Prefers symbols-only fonts,
/// then `Mono` variants; ties are broken alphabetically so the choice is always the same.
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

/// A variant of the primary font (bold, italic...) loaded on demand. A Nerd Font weighs about
/// 2.5 MB with thousands of glyphs and fontdue processes all of them on load: with all four
/// variants that meant several seconds of startup time.
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

/// Private Use Area (BMP and Supplementary A), where Nerd Fonts place their icons
fn is_private_use(ch: char) -> bool {
    matches!(ch, '\u{E000}'..='\u{F8FF}' | '\u{F0000}'..='\u{FFFFD}')
}

struct Fonts {
    regular_id: ID,
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
        let (regular_id, regular) = find_face(db, &config.family, false, false)
            .and_then(|id| Some((id, load_face(db, id, px)?)))
            .ok_or_else(|| format!("font \"{}\" not found on the system", config.family))?;

        let mut fallbacks: Vec<LazyFont> =
            config.fallback.iter().cloned().map(LazyFont::new).collect();
        // Last resort: any installed Nerd Font, only for what nobody else has
        if let Some(nerd) = detect_nerd_font(db)
            && nerd != config.family
            && !config.fallback.contains(&nerd)
        {
            fallbacks.push(LazyFont::new(nerd));
        }

        Ok(Self {
            regular_id,
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
        // Glyphs are always looked up in the Unicode cmap (`unicode_glyph`) and drawn by index:
        // fontdue's own char lookup can return the Mac Roman glyph instead
        let styled = match (key.bold, key.italic) {
            (false, false) => None,
            (true, false) => Some(&mut self.bold),
            (false, true) => Some(&mut self.italic),
            (true, true) => Some(&mut self.bold_italic),
        };
        if let Some(face) = styled
            && let Some(id) = face.id
            && let Some(glyph) = unicode_glyph(db, id, key.ch)
            && let Some(font) = face.get(db, px)
        {
            return font.rasterize_indexed(glyph, px);
        }
        if let Some(glyph) = unicode_glyph(db, self.regular_id, key.ch) {
            return self.regular.rasterize_indexed(glyph, px);
        }

        // The fallbacks, in order. In the Private Use Area the system's CJK and symbol fonts ship
        // legacy glyphs that have nothing to do with Nerd Font icons, so Nerd Fonts go first
        // there. `covers` is cheap and avoids loading a huge font just to find out it lacks the
        // character.
        let private_use = is_private_use(key.ch);
        for nerd_pass in [true, false] {
            if nerd_pass && !private_use {
                continue;
            }
            for slot in &mut self.fallbacks {
                if nerd_pass && !slot.nerd {
                    continue;
                }
                if let Some(glyph) = slot.glyph(db, key.ch)
                    && let Some(font) = slot.get(db, px)
                {
                    return font.rasterize_indexed(glyph, px);
                }
            }
        }
        // Glyph 0 = .notdef (the "character not available" box)
        self.regular.rasterize_indexed(0, px)
    }
}

pub struct GlyphCache {
    /// Fonts installed on the system, scanned only once
    db: Database,
    font: FontConfig,
    scale_factor: f64,
    fonts: Fonts,
    pub cell_width: u32,
    pub cell_height: u32,
    /// Thickness of underlines, strikethrough, and cursor
    pub thickness: u32,
    /// Distances from the top edge of the cell
    pub underline_y: u32,
    pub strikethrough_y: u32,
    baseline: u32,
    texture: wgpu::Texture,
    queue: wgpu::Queue,
    glyphs: HashMap<GlyphKey, Option<Glyph>>,
    // Row packing: next free position and height of the current row
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
            label: Some("glyph atlas"),
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
        self.texture
            .create_view(&wgpu::TextureViewDescriptor::default())
    }

    /// Reloads the fonts at the new pixel size and clears the atlas
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        if let Ok(fonts) = Fonts::load(&self.db, &self.font, scale_factor) {
            self.scale_factor = scale_factor;
            self.install(fonts);
        }
    }

    /// Switches fonts (when the configuration is reloaded). Atomic: if the family does not exist,
    /// it returns the error and keeps the previous font. Does nothing if nothing changed.
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

    /// Reports (only once) whether the atlas filled up and was cleared during the current frame.
    /// Instances built before the clear point at stale positions and must be rebuilt.
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
            .map_or((px * 0.8, -px * 0.2, 0.0), |m| {
                (m.ascent, m.descent, m.line_gap)
            });
        self.cell_width = font.metrics('0', px).advance_width.round().max(1.0) as u32;
        self.cell_height = (ascent - descent + gap).ceil().max(1.0) as u32;
        self.baseline = (ascent + gap / 2.0).round().max(0.0) as u32;
        self.thickness = (px / 14.0).round().max(1.0) as u32;
        self.underline_y = (self.baseline
            + (self.cell_height - self.baseline.min(self.cell_height)) / 3)
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
            return None; // spaces and the like: nothing to draw
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
            // ymin is the bitmap's bottom edge relative to the baseline (positive upward)
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

    // Powerline (separator) and the Font Awesome home icon, both in the Private Use Area
    const POWERLINE: char = '\u{E0B0}';
    const HOME_ICON: char = '\u{F015}';
    // Nerd Fonts v3 Material Design, in the Supplementary Private Use Area
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

    /// The glyph was actually drawn: it has a bitmap and is not the primary font's .notdef box
    fn assert_drawn(fonts: &mut Fonts, db: &Database, ch: char) {
        let (metrics, bitmap) = fonts.rasterize(db, plain(ch));
        let (notdef_metrics, notdef_bitmap) = fonts.regular.rasterize_indexed(0, fonts.px);
        assert!(
            metrics.width > 0 && bitmap.iter().any(|&b| b > 0),
            "{ch:?} came out empty"
        );
        let is_notdef = metrics.width == notdef_metrics.width
            && metrics.height == notdef_metrics.height
            && bitmap == notdef_bitmap;
        assert!(!is_notdef, "{ch:?} came out as .notdef");
    }

    #[test]
    fn a_nerd_font_as_the_primary_font_draws_icons() {
        let db = database();
        let family = "JetBrainsMono Nerd Font Mono";
        if !has_family(&db, family) {
            eprintln!("skipped: {family} is not installed");
            return;
        }
        let mut fonts = Fonts::load(&db, &config(family), 1.0).unwrap();
        for ch in [POWERLINE, HOME_ICON, MATERIAL_ICON] {
            assert_drawn(&mut fonts, &db, ch);
        }
    }

    #[test]
    fn icons_are_found_in_an_installed_nerd_font_when_the_primary_font_lacks_them() {
        // Consolas ships no icons; the last resort is any installed Nerd Font
        let db = database();
        if !has_family(&db, "Consolas") || detect_nerd_font(&db).is_none() {
            eprintln!("skipped: requires Consolas and a Nerd Font");
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
            eprintln!("skipped: requires Consolas and {nerd}");
            return;
        }
        // With the Nerd Font in the explicit fallback list (previously it was skipped for these
        // code points)
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
            eprintln!("skipped: Microsoft YaHei is not installed");
            return;
        }
        let mut slot = LazyFont::new("Microsoft YaHei".to_string());
        assert!(slot.covers(&db, '日'));
        assert!(!slot.covers(&db, POWERLINE));
        assert!(
            slot.font.is_none(),
            "covers must not load the font (it weighs tens of MB)"
        );
        assert!(slot.get(&db, 16.0).is_some());
        assert!(slot.font.is_some());
    }

    #[test]
    fn nerd_fonts_win_over_system_fonts_for_private_use_icons() {
        // Malgun Gothic, YaHei and Segoe UI Symbol ship legacy glyphs in the Private Use Area:
        // even when they are in the fallback list, an icon must come from the Nerd Font
        let db = database();
        let Some(nerd) = detect_nerd_font(&db) else {
            eprintln!("skipped: no Nerd Font is installed");
            return;
        };
        if !has_family(&db, "Consolas") || !has_family(&db, "Malgun Gothic") {
            eprintln!("skipped: requires Consolas and Malgun Gothic");
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
            assert_eq!(
                (metrics.width, metrics.height),
                (expected_metrics.width, expected_metrics.height),
                "{ch:?}"
            );
            assert_eq!(bitmap, expected, "{ch:?} did not come from {nerd}");
        }
    }

    #[test]
    fn bold_and_italic_faces_are_loaded_only_when_needed() {
        let db = database();
        if !has_family(&db, "Consolas") {
            eprintln!("skipped: Consolas is not installed");
            return;
        }
        let mut fonts = Fonts::load(&db, &config("Consolas"), 1.0).unwrap();
        assert!(
            fonts.bold.font.is_none()
                && fonts.italic.font.is_none()
                && fonts.bold_italic.font.is_none()
        );

        fonts.rasterize(&db, plain('a'));
        assert!(
            fonts.bold.font.is_none(),
            "a regular letter must not load the bold face"
        );

        fonts.rasterize(
            &db,
            GlyphKey {
                ch: 'a',
                bold: true,
                italic: false,
            },
        );
        assert!(
            fonts.bold.font.is_some(),
            "the bold face is loaded when used"
        );
        assert!(fonts.italic.font.is_none() && fonts.bold_italic.font.is_none());
    }

    #[test]
    fn latin1_characters_use_the_unicode_cmap_not_the_mac_roman_one() {
        // Menlo also ships a Mac Roman cmap: read naively, 'ó' (U+00F3) is drawn as byte 0xF3
        // in Mac Roman, which is 'Û'
        let db = database();
        let Some(id) = find_face(&db, "Menlo", false, false) else {
            eprintln!("skipped: Menlo is not installed");
            return;
        };
        let mut fonts = Fonts::load(&db, &config("Menlo"), 1.0).unwrap();
        for ch in ['ó', 'é', 'á', 'ñ', '×', '¿', '·'] {
            let index = db
                .with_face_data(id, |data, index| {
                    ttf_parser::Face::parse(data, index).ok()?.glyph_index(ch)
                })
                .flatten()
                .unwrap_or_else(|| panic!("Menlo should have {ch:?}"));
            let (metrics, bitmap) = fonts.rasterize(&db, plain(ch));
            let (expected_metrics, expected) = fonts.regular.rasterize_indexed(index.0, fonts.px);
            assert_eq!(
                (metrics.width, metrics.height),
                (expected_metrics.width, expected_metrics.height),
                "{ch:?}"
            );
            assert_eq!(
                bitmap, expected,
                "{ch:?} was not drawn with its Unicode glyph"
            );
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_default_macos_fonts_draw_media_control_symbols() {
        // Claude Code and other TUIs use ⏺ and ⏵: on macOS only STIX Two Math has them as
        // outlines (Apple Color Emoji is a bitmap font that fontdue cannot rasterize)
        let db = database();
        let mut fonts = Fonts::load(&db, &FontConfig::default(), 1.0).unwrap();
        for ch in ['\u{23FA}', '\u{23F5}'] {
            assert_drawn(&mut fonts, &db, ch);
        }
    }

    #[test]
    fn private_use_ranges_are_recognized() {
        assert!(
            is_private_use('\u{E0B0}') && is_private_use('\u{F8FF}') && is_private_use('\u{F0001}')
        );
        assert!(!is_private_use('a') && !is_private_use('日') && !is_private_use('\u{1F600}'));
    }

    #[test]
    fn a_missing_family_covers_nothing() {
        let db = database();
        let mut slot = LazyFont::new("Nonexistent Font Family".to_string());
        assert!(!slot.covers(&db, 'a'));
        assert!(slot.get(&db, 16.0).is_none());
    }

    #[test]
    fn the_automatic_nerd_font_is_deterministic_and_prefers_mono_variants() {
        let db = database();
        let Some(chosen) = detect_nerd_font(&db) else {
            eprintln!("skipped: no Nerd Font is installed");
            return;
        };
        assert_eq!(
            detect_nerd_font(&db),
            Some(chosen.clone()),
            "must always pick the same one"
        );
        assert!(chosen.contains("Nerd Font"), "{chosen}");
        let any_symbols = db.faces().any(|f| {
            f.families
                .iter()
                .any(|(n, _)| n.starts_with("Symbols") && n.contains("Nerd Font"))
        });
        let any_mono = db.faces().any(|f| {
            f.families
                .iter()
                .any(|(n, _)| n.ends_with("Nerd Font Mono"))
        });
        if any_symbols {
            assert!(chosen.starts_with("Symbols"), "{chosen}");
        } else if any_mono {
            assert!(chosen.ends_with("Nerd Font Mono"), "{chosen}");
        }
    }
}
