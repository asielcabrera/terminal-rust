//! Draws the terminal grid with wgpu: one instanced quad per rectangle (backgrounds, underlines,
//! cursor) and per glyph, all in a single render pass.

use std::collections::HashMap;
use std::mem::size_of;
use std::ops::Range;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use libghostty_vt::kitty::graphics::{self, Compression, ImageFormat};
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

/// A pane's instances within the frame: their ranges in the vectors and the area they are
/// clipped to. Each pane is drawn in full (backgrounds, images below the text, glyphs, and
/// decorations) before the next one, so whatever is on top completely covers what is under.
#[derive(Clone)]
struct Batch {
    scissor: Rect,
    backgrounds: Range<u32>,
    images_below: Range<u32>,
    glyphs: Range<u32>,
    overlays: Range<u32>,
    images_above: Range<u32>,
}

/// A frame's instances. The vectors are reused across frames to avoid allocating every time.
#[derive(Default)]
struct Frame {
    backgrounds: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    overlays: Vec<RectInstance>,
    images: Vec<ImageInstance>,
    image_keys: Vec<(u32, u64)>,
    batches: Vec<Batch>,
    text: String,
}

impl Frame {
    fn clear(&mut self) {
        self.backgrounds.clear();
        self.glyphs.clear();
        self.overlays.clear();
        self.images.clear();
        self.image_keys.clear();
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

/// One kitty graphics placement: an instanced quad textured with the image's pixels. The uv
/// interval covers the placement's source rectangle, restricted to what the viewport clip left
/// visible, so a partially scrolled image samples exactly the part that remains on screen.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ImageInstance {
    pos: [f32; 2],
    size: [f32; 2],
    uv_origin: [f32; 2],
    uv_size: [f32; 2],
}

/// The visible rectangle of a placement, in pane-grid pixels, and the uv window that maps to it
#[derive(Debug, Clone, Copy, PartialEq)]
struct ImageQuad {
    x: i64,
    y: i64,
    w: u64,
    h: u64,
    uv_origin: [f32; 2],
    uv_size: [f32; 2],
}

/// Destination rectangle (in pane-grid pixels, top-left origin) and texture uv window of one
/// image placement, clipped to the visible grid. The row of a scrolled placement may be
/// negative: whatever falls above the grid is cut and the uv window moves down by the same
/// proportion, so the bottom of the texture fills the visible part. `None` when nothing of the
/// placement is visible or a dimension is degenerate.
#[allow(clippy::too_many_arguments)]
fn image_quad(
    viewport: (i32, i32),
    offset: (u32, u32),
    pixel: (u32, u32),
    source: (u32, u32, u32, u32),
    image: (u32, u32),
    cell: (u32, u32),
    grid: (u16, u16),
) -> Option<ImageQuad> {
    if pixel.0 == 0 || pixel.1 == 0 || image.0 == 0 || image.1 == 0 || cell.0 == 0 || cell.1 == 0 {
        return None;
    }
    let (grid_w, grid_h) = (
        i64::from(grid.0) * i64::from(cell.0),
        i64::from(grid.1) * i64::from(cell.1),
    );
    let (dx, dy) = (
        i64::from(viewport.0) * i64::from(cell.0) + i64::from(offset.0),
        i64::from(viewport.1) * i64::from(cell.1) + i64::from(offset.1),
    );
    let x0 = dx.clamp(0, grid_w);
    let x1 = (dx + i64::from(pixel.0)).clamp(0, grid_w);
    let y0 = dy.clamp(0, grid_h);
    let y1 = (dy + i64::from(pixel.1)).clamp(0, grid_h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    // Fractions of the rendered quad that survived the clip, applied to the source rect
    let (pw, ph) = (pixel.0 as f32, pixel.1 as f32);
    let fx0 = (x0 - dx) as f32 / pw;
    let fx1 = (x1 - dx) as f32 / pw;
    let fy0 = (y0 - dy) as f32 / ph;
    let fy1 = (y1 - dy) as f32 / ph;
    let (iw, ih) = (image.0 as f32, image.1 as f32);
    let (sx, sy, sw, sh) = (
        source.0 as f32,
        source.1 as f32,
        source.2 as f32,
        source.3 as f32,
    );
    Some(ImageQuad {
        x: x0,
        y: y0,
        w: (x1 - x0) as u64,
        h: (y1 - y0) as u64,
        uv_origin: [(sx + sw * fx0) / iw, (sy + sh * fy0) / ih],
        uv_size: [sw * (fx1 - fx0) / iw, sh * (fy1 - fy0) / ih],
    })
}

/// The kitty unicode-placeholder codepoint: a cell holding it (plus diacritic combining marks)
/// shows a slice of an image instead of text.
const KITTY_PLACEHOLDER: u32 = 0x10_EEEE;

/// GPU memory budget for kitty image textures
const IMAGE_CACHE_BYTES: u64 = 512 * 1024 * 1024;
/// Frames a texture may stay unused before it is evicted regardless of the cap
const IMAGE_CACHE_KEEP_FRAMES: u64 = 300;

/// Pure bookkeeping for the image texture cache: which images are on the GPU and how many
/// bytes they hold. The policy, enforced on every touch:
///
/// 1. A texture unused for `keep_frames` frames is evicted (the image scrolled away or was
///    deleted: holding it serves nobody).
/// 2. While the total exceeds `cap` bytes, the least recently used textures are evicted. The
///    key just touched is never evicted, so a single image bigger than the cap still renders
///    (displacing everything else).
///
/// Evicted textures are re-uploaded from the terminal's image storage when they are seen
/// again, so eviction costs bandwidth, never correctness. Keys are `(image id, generation)`:
/// a retransmission of the same id gets a fresh generation stamp and thus a fresh entry, so a
/// stale texture can never be shown for changed pixels.
struct TextureLedger {
    entries: HashMap<(u32, u64), LedgerEntry>,
    bytes: u64,
    frame: u64,
    cap: u64,
    keep_frames: u64,
}

#[derive(Clone, Copy)]
struct LedgerEntry {
    bytes: u64,
    last_used: u64,
}

impl TextureLedger {
    fn new(cap: u64, keep_frames: u64) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            frame: 1,
            cap,
            keep_frames,
        }
    }

    fn advance_frame(&mut self) {
        self.frame += 1;
    }

    fn is_cached(&self, key: (u32, u64)) -> bool {
        self.entries.contains_key(&key)
    }

    /// Marks `key` (holding `bytes`) as used this frame; returns the keys whose GPU textures
    /// must be dropped, stale ones first, then least-recently-used ones until the cache fits
    /// the cap.
    fn touch(&mut self, key: (u32, u64), bytes: u64) -> Vec<(u32, u64)> {
        let frame = self.frame;
        let previous = self.entries.insert(
            key,
            LedgerEntry {
                bytes,
                last_used: frame,
            },
        );
        self.bytes += bytes - previous.map_or(0, |entry| entry.bytes);
        let mut evicted = Vec::new();
        let stale: Vec<(u32, u64)> = self
            .entries
            .iter()
            .filter(|(k, entry)| **k != key && frame - entry.last_used >= self.keep_frames)
            .map(|(k, _)| *k)
            .collect();
        for victim in stale {
            self.drop_entry(&mut evicted, victim);
        }
        while self.bytes > self.cap {
            let Some(victim) = self
                .entries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(k, entry)| (entry.last_used, **k))
                .map(|(k, _)| *k)
            else {
                break;
            };
            self.drop_entry(&mut evicted, victim);
        }
        evicted
    }

    /// Forgets a key whose upload did not happen (malformed pixels), freeing its bytes
    fn remove(&mut self, key: (u32, u64)) {
        let mut dropped = Vec::new();
        self.drop_entry(&mut dropped, key);
    }

    fn drop_entry(&mut self, evicted: &mut Vec<(u32, u64)>, key: (u32, u64)) {
        if let Some(entry) = self.entries.remove(&key) {
            self.bytes -= entry.bytes;
            evicted.push(key);
        }
    }
}

/// Pixels of one image, ready for the GPU: the decoder produces (and direct transmits carry)
/// 8-bit data which is expanded here to the RGBA8 the texture wants
struct ImageUpload {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Bytes per pixel of a format the pipeline can render; `None` for what it cannot (stored PNG
/// would not have been decoded, zlib-deflated raw transmits are not decompressed here: PNG,
/// `f=100`, is the compressed path in use and it is decoded on transmit)
fn renderable_chunk(format: ImageFormat, compression: Compression) -> Option<usize> {
    if compression != Compression::None {
        return None;
    }
    match format {
        ImageFormat::Rgba => Some(4),
        ImageFormat::Rgb => Some(3),
        ImageFormat::GrayAlpha => Some(2),
        ImageFormat::Gray => Some(1),
        _ => None,
    }
}

/// Expands the stored pixels to RGBA8. Rejects malformed data (too short for the geometry).
fn image_pixels(
    format: ImageFormat,
    compression: Compression,
    width: u32,
    height: u32,
    data: &[u8],
) -> Option<Vec<u8>> {
    let chunk = renderable_chunk(format, compression)?;
    let pixels = width as usize * height as usize;
    if width == 0 || height == 0 || data.len() < pixels * chunk {
        return None;
    }
    if chunk == 4 {
        return Some(data[..pixels * 4].to_vec());
    }
    let mut out = Vec::with_capacity(pixels * 4);
    for px in data[..pixels * chunk].chunks_exact(chunk) {
        let (r, g, b, a) = match format {
            ImageFormat::Rgb => (px[0], px[1], px[2], 255),
            ImageFormat::Gray => (px[0], px[0], px[0], 255),
            ImageFormat::GrayAlpha => (px[0], px[0], px[0], px[1]),
            _ => return None,
        };
        out.extend_from_slice(&[r, g, b, a]);
    }
    Some(out)
}

/// A virtual (unicode placeholder) placement as libghostty stores it: the reference grid every
/// placeholder run for `(image id, placement id)` resolves against. Its grid position does not
/// exist in the storage — it lives in the placeholder cells themselves.
struct VirtualRef {
    /// Requested grid size (`c=`/`r=` of the placement); 0 means "fit the image"
    columns: u32,
    rows: u32,
    z: i32,
    /// Texture cache key of the referenced image
    key: (u32, u64),
    /// Image dimensions in pixels
    image: (u32, u32),
}

/// Diacritic combining marks used to encode the fragment coordinates of a placeholder cell,
/// sorted; the index into this table is the value they encode. From libghostty's
/// `graphics_unicode.zig` (kitty's rowcolumn-diacritics table).
static DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// The value a diacritic combining mark encodes (`None` when it is not one of kitty's)
fn diacritic_index(cp: u32) -> Option<u32> {
    DIACRITICS.binary_search(&cp).ok().map(|index| index as u32)
}

/// The kitty image id encoded in a style color: the 24 most significant bits, which works for
/// both palette and RGB colors (libghostty's `colorToId`)
fn color_to_id(color: StyleColor) -> u32 {
    match color {
        StyleColor::None => 0,
        StyleColor::Palette(index) => u32::from(index.0),
        StyleColor::Rgb(rgb) => {
            (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
        }
    }
}

/// The destination rectangle and source rectangle of one virtual placement run: where the
/// run's cells sit relative to its top-left cell, and what part of the image fills them.
/// Verbatim port of libghostty's `graphics_unicode.zig::Placement.renderPlacement`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct VirtualRect {
    /// Pixel offsets from the top-left corner of the run's starting cell
    offset_x: i64,
    offset_y: i64,
    /// Destination size in pixels
    dest_w: u64,
    dest_h: u64,
    /// Source rectangle in image pixels (what to sample)
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
}

/// Computes the render placement of one virtual placement run. `columns`/`rows` are the
/// placement's grid size (0 = fit the image at one pixel per... at whole-image size),
/// `col`/`row` the 0-indexed fragment of the image the run shows, and `width` the run width in
/// cells (a run is always one cell tall).
#[allow(clippy::too_many_arguments)]
fn virtual_render(
    columns: u32,
    rows: u32,
    col: u32,
    row: u32,
    width: u32,
    image: (u32, u32),
    cell: (u32, u32),
) -> Option<VirtualRect> {
    if image.0 == 0 || image.1 == 0 || cell.0 == 0 || cell.1 == 0 || width == 0 {
        return None;
    }
    // The placement's grid size: the requested rows/columns, or, when unspecified, a grid just
    // big enough to show the whole image at its original size.
    let mut grid_cols = columns;
    let mut grid_rows = rows;
    if grid_cols == 0 {
        grid_cols = image.0.div_ceil(cell.0);
    }
    if grid_rows == 0 {
        grid_rows = image.1.div_ceil(cell.1);
    }
    let (img_w, img_h) = (f64::from(image.0), f64::from(image.1));
    let (p_cols_px, p_rows_px) = (
        f64::from(grid_cols) * f64::from(cell.0),
        f64::from(grid_rows) * f64::from(cell.1),
    );

    // Fit the image into the placement grid, preserving the aspect ratio, centered along the
    // dimension that has slack.
    let (x_scale, y_scale, x_offset, y_offset);
    if img_w * p_rows_px > img_h * p_cols_px {
        // Image is wider than the grid: fit width, center height
        x_scale = p_cols_px / img_w.max(1.0);
        y_scale = x_scale;
        y_offset = (p_rows_px - img_h * y_scale) / 2.0;
        x_offset = 0.0;
    } else {
        // Image is taller than the grid: fit height, center width
        y_scale = p_rows_px / img_h.max(1.0);
        x_scale = y_scale;
        x_offset = (p_cols_px - img_w * x_scale) / 2.0;
        y_offset = 0.0;
    }
    // The scaled image: offsets from its true pixels to the centered bounding box
    let scaled = (
        x_offset / x_scale,
        y_offset / y_scale,
        img_w + (x_offset / x_scale) * 2.0,
        img_h + (y_offset / y_scale) * 2.0,
    );

    // This run's slice of the scaled image (a run is one cell tall)
    let mut src_w = scaled.2 * (f64::from(width) / f64::from(grid_cols));
    let mut src_h = scaled.3 / f64::from(grid_rows);
    let mut src_x = scaled.2 * (f64::from(col) / f64::from(grid_cols));
    let mut src_y = scaled.3 * (f64::from(row) / f64::from(grid_rows));
    let mut dest_w = f64::from(width) * f64::from(cell.0);
    let mut dest_h = f64::from(cell.1);
    let mut dest_x_offset = 0.0;
    let mut dest_y_offset = 0.0;

    // Align the source with the centered image: the bands beyond the real image are blank, so
    // slices that fall into them lose source area and gain a pixel offset.
    if src_y < scaled.1 {
        let offset = scaled.1 - src_y;
        src_h -= offset;
        dest_y_offset = offset;
        dest_h -= offset * y_scale;
        src_y = 0.0;
        if src_h > img_h {
            src_h = img_h;
            dest_h = img_h * y_scale;
        }
    } else if src_y + src_h > scaled.3 - scaled.1 {
        src_y -= scaled.1;
        src_h = scaled.3 - scaled.1 - src_y;
        src_h -= scaled.1;
        dest_h = src_h * y_scale;
    } else {
        src_y -= scaled.1;
    }
    if src_x < scaled.0 {
        let offset = scaled.0 - src_x;
        src_w -= offset;
        dest_x_offset = offset;
        dest_w -= offset * x_scale;
        src_x = 0.0;
        if src_w > img_w {
            src_w = img_w;
            dest_w = img_w * x_scale;
        }
    } else if src_x + src_w > scaled.2 - scaled.0 {
        src_x -= scaled.0;
        src_w = scaled.2 - scaled.0 - src_x;
        src_w -= scaled.0;
        dest_w = src_w * x_scale;
    } else {
        src_x -= scaled.0;
    }

    // Nothing visible: the slice is an entirely blank band (libghostty's rounding of the same
    // degenerate case would go negative; a blank run draws nothing instead).
    if src_w <= 0.0 || src_h <= 0.0 || dest_w <= 0.0 || dest_h <= 0.0 {
        return None;
    }
    Some(VirtualRect {
        offset_x: (dest_x_offset * x_scale).round() as i64,
        offset_y: (dest_y_offset * y_scale).round() as i64,
        dest_w: dest_w.round() as u64,
        dest_h: dest_h.round() as u64,
        src_x: src_x.round() as u32,
        src_y: src_y.round() as u32,
        src_w: src_w.round() as u32,
        src_h: src_h.round() as u32,
    })
}

/// One placement ready to draw: its clipped quad (grid-relative) and, when the texture is not
/// on the GPU yet, the pixels to upload. Which z layer it belongs to is encoded by which vector
/// (`below` / `above`) holds it.
struct ImageDraw {
    key: (u32, u64),
    quad: ImageQuad,
    upload: Option<ImageUpload>,
}

/// The kitty images of one pane, split by z layer: `z < 0` goes below the text, `z >= 0`
/// (the iterator's `Layer::AboveText`) goes on top of it. `evicted` lists the texture keys the
/// ledger dropped this frame.
#[derive(Default)]
struct PaneImages {
    below: Vec<ImageDraw>,
    above: Vec<ImageDraw>,
    /// Pixel uploads for images referenced only by virtual placements (their quads come later,
    /// from the run scan) — one per texture key, applied before any drawing
    uploads: Vec<((u32, u64), ImageUpload)>,
    evicted: Vec<(u32, u64)>,
    virtuals: HashMap<(u32, u32), VirtualRef>,
    /// Image id → lowest placement id stored for it, for placeholder cells that name no
    /// placement (libghostty resolves those to "some virtual placement of the image"; the
    /// lowest id keeps the choice deterministic)
    fallback: HashMap<u32, u32>,
}

/// Reads every visible placement of the pane's active screen and turns it into drawables.
/// Virtual (unicode placeholder) placements draw nothing (the text cell is empty anyway),
/// fully off-screen ones are skipped by libghostty itself. Pixel data is copied only when a
/// texture must be uploaded; geometry is recomputed every frame because scrolling moves
/// placements without touching the storage.
fn collect_images(
    terminal: &Terminal<'static, 'static>,
    graphics: &graphics::Graphics<'_>,
    placement_iter: &mut graphics::PlacementIterator<'static>,
    ledger: &mut TextureLedger,
    cell: (u32, u32),
    grid: (u16, u16),
) -> PaneImages {
    let mut out = PaneImages::default();
    let Ok(mut placements) = placement_iter.update(graphics) else {
        return out;
    };
    while let Some(placement) = placements.next() {
        let (Ok(image_id), Ok(is_virtual), Ok(z)) =
            (placement.image_id(), placement.is_virtual(), placement.z())
        else {
            continue;
        };
        let Some(image) = graphics.image(image_id) else {
            continue;
        };
        let (Ok(format), Ok(compression), Ok(width), Ok(height), Ok(generation)) = (
            image.format(),
            image.compression(),
            image.width(),
            image.height(),
            image.generation(),
        ) else {
            continue;
        };
        // Renderability decides whether the key enters the ledger at all
        if renderable_chunk(format, compression).is_none() {
            continue;
        }
        let key = (image_id, generation);

        if is_virtual {
            // The iterator yields the virtual placement's grid size and z, but not a position:
            // it is rendered wherever its unicode placeholder cells sit in the grid (see
            // `build_image_runs`). Only its pixels and reference are needed here.
            let (Ok(placement_cols), Ok(placement_rows), Ok(placement_id)) = (
                placement.columns(),
                placement.rows(),
                placement.placement_id(),
            ) else {
                continue;
            };
            let bytes = u64::from(width) * u64::from(height) * 4;
            let fresh = !ledger.is_cached(key);
            out.evicted.extend(ledger.touch(key, bytes));
            if fresh {
                match image.data().ok().and_then(|data| {
                    image_pixels(format, compression, width, height, data).map(|rgba| ImageUpload {
                        width,
                        height,
                        rgba,
                    })
                }) {
                    Some(upload) => out.uploads.push((key, upload)),
                    // Malformed: nothing is drawn and the ledger must not reserve for it
                    None => {
                        ledger.remove(key);
                        continue;
                    }
                }
            }
            out.virtuals
                .entry((image_id, placement_id))
                .or_insert(VirtualRef {
                    columns: placement_cols,
                    rows: placement_rows,
                    z,
                    key,
                    image: (width, height),
                });
            let slot = out.fallback.entry(image_id).or_insert(placement_id);
            if placement_id < *slot {
                *slot = placement_id;
            }
            continue;
        }
        let Ok(info) = placement.placement_render_info(&image, terminal) else {
            continue;
        };
        if !info.viewport_visible {
            continue;
        }
        let Some(quad) = image_quad(
            (info.viewport_col, info.viewport_row),
            (
                placement.x_offset().unwrap_or(0),
                placement.y_offset().unwrap_or(0),
            ),
            (info.pixel_width, info.pixel_height),
            (
                info.source_x,
                info.source_y,
                info.source_width,
                info.source_height,
            ),
            (width, height),
            cell,
            grid,
        ) else {
            continue;
        };

        let fresh = !ledger.is_cached(key);
        out.evicted
            .extend(ledger.touch(key, u64::from(width) * u64::from(height) * 4));
        let upload = if fresh {
            let upload = image.data().ok().and_then(|data| {
                image_pixels(format, compression, width, height, data).map(|rgba| ImageUpload {
                    width,
                    height,
                    rgba,
                })
            });
            if upload.is_none() {
                // Malformed: nothing is drawn and the ledger must not reserve for it
                ledger.remove(key);
                continue;
            }
            upload
        } else {
            None
        };
        let draw = ImageDraw { key, quad, upload };
        if z >= 0 {
            out.above.push(draw);
        } else {
            out.below.push(draw);
        }
    }
    out
}

/// A decoded placeholder cell: everything one U+10EEEE cell says about the placement it
/// belongs to. The image id low part comes from the fg color, the optional high part and the
/// image fragment coordinates from diacritic combining marks, and the optional placement id
/// from the underline color.
struct RunCell {
    image_id_low: u32,
    image_id_high: Option<u32>,
    placement_id: u32,
    frag_row: Option<u32>,
    frag_col: Option<u32>,
}

/// An incomplete run of consecutive placeholder cells (libghostty's `IncompletePlacement`)
struct RunPartial {
    /// Viewport cell where the run's first placeholder sits
    start_col: u16,
    start_row: u16,
    cell: RunCell,
    /// Effective fragment origin (diacritics may be omitted; they default to 0)
    fragment: (u32, u32),
    /// Run width so far, in cells
    width: u32,
}

/// Does `next` continue the run `prev`? Port of libghostty's `canAppend`: same image id (low
/// part), same placement id, equal fragment row (or omitted), the fragment column continuing
/// the run (or omitted), and the same optional high id part.
fn runs_match(prev: &RunPartial, next: &RunCell) -> bool {
    next.image_id_low == prev.cell.image_id_low
        && next.placement_id == prev.cell.placement_id
        && (next.frag_row.is_none() || next.frag_row == Some(prev.fragment.0))
        && (next.frag_col.is_none() || next.frag_col == Some(prev.fragment.1 + prev.width))
        && (next.image_id_high.is_none() || next.image_id_high == prev.cell.image_id_high)
}

/// Scans the pane's visible grid for rows holding kitty unicode placeholders and turns every
/// consecutive run of placeholder cells into an image draw showing its slice of the referenced
/// image. Placeholder cells draw no glyphs of their own (the font has no U+10EEEE and the
/// diacritics are Combining marks): `build_frame` suppresses their text via the same row flag
/// this scan keys on.
fn build_image_runs(
    snapshot: &Snapshot<'static, '_>,
    row_iter: &mut RowIterator<'static>,
    cell_iter: &mut CellIterator<'static>,
    virtuals: &HashMap<(u32, u32), VirtualRef>,
    fallback: &HashMap<u32, u32>,
    cell: (u32, u32),
) -> (Vec<ImageDraw>, Vec<ImageDraw>) {
    let mut below = Vec::new();
    let mut above = Vec::new();

    /// Resolves the virtual placement a run references: the exact (image, placement) pair when
    /// the run names one; otherwise the image's lowest placement id (libghostty resolves those
    /// to "some virtual placement of the image"; the lowest id keeps the choice deterministic).
    fn lookup<'a>(
        virtuals: &'a HashMap<(u32, u32), VirtualRef>,
        fallback: &HashMap<u32, u32>,
        image_id: u32,
        placement_id: u32,
    ) -> Option<&'a VirtualRef> {
        virtuals.get(&(image_id, placement_id)).or_else(|| {
            fallback
                .get(&image_id)
                .and_then(|pid| virtuals.get(&(image_id, *pid)))
        })
    }

    /// Turns a completed run into an image draw, if its image exists and offers a visible slice
    fn emit(
        run: &RunPartial,
        virtuals: &HashMap<(u32, u32), VirtualRef>,
        fallback: &HashMap<u32, u32>,
        cell: (u32, u32),
    ) -> Option<ImageDraw> {
        let image_id = run.cell.image_id_low | (run.cell.image_id_high.unwrap_or(0) << 24);
        let reference = lookup(virtuals, fallback, image_id, run.cell.placement_id)?;
        let rect = virtual_render(
            reference.columns,
            reference.rows,
            run.fragment.1,
            run.fragment.0,
            run.width,
            reference.image,
            cell,
        )?;
        if rect.src_w == 0 || rect.src_h == 0 || rect.dest_w == 0 || rect.dest_h == 0 {
            return None;
        }
        let (img_w, img_h) = (reference.image.0 as f32, reference.image.1 as f32);
        Some(ImageDraw {
            key: reference.key,
            quad: ImageQuad {
                x: i64::from(run.start_col) * i64::from(cell.0) + rect.offset_x,
                y: i64::from(run.start_row) * i64::from(cell.1) + rect.offset_y,
                w: rect.dest_w,
                h: rect.dest_h,
                uv_origin: [rect.src_x as f32 / img_w, rect.src_y as f32 / img_h],
                uv_size: [rect.src_w as f32 / img_w, rect.src_h as f32 / img_h],
            },
            upload: None,
        })
    }

    /// Emits a completed run into its z layer
    fn emit_run(
        prev: RunPartial,
        virtuals: &HashMap<(u32, u32), VirtualRef>,
        fallback: &HashMap<u32, u32>,
        cell: (u32, u32),
        below: &mut Vec<ImageDraw>,
        above: &mut Vec<ImageDraw>,
    ) {
        if let Some(draw) = emit(&prev, virtuals, fallback, cell)
            && let Some(reference) = lookup(
                virtuals,
                fallback,
                prev.cell.image_id_low | (prev.cell.image_id_high.unwrap_or(0) << 24),
                prev.cell.placement_id,
            )
        {
            if reference.z < 0 {
                below.push(draw);
            } else {
                above.push(draw);
            }
        }
    }

    let Ok(mut rows) = row_iter.update(snapshot) else {
        return (below, above);
    };
    let mut partial: Option<RunPartial> = None;
    let mut graphemes: Vec<char> = Vec::new();
    let mut row_index = 0u16;
    while let Some(row) = rows.next() {
        let this_row = row_index;
        row_index += 1;
        let has_placeholders = row
            .raw_row()
            .ok()
            .and_then(|raw| raw.has_kitty_virtual_placeholder().ok())
            .unwrap_or(false);
        if !has_placeholders {
            continue;
        }
        let Ok(mut cells) = cell_iter.update(row) else {
            continue;
        };
        let mut col = 0u16;
        while let Some(iterated) = cells.next() {
            let this_col = col;
            col += 1;
            // Decode the placeholder, if this cell is one. The grapheme cluster holds the
            // placeholder itself, then the diacritics, positionally: row, column, id high byte.
            // An invalid diacritic marks its slot as absent (libghostty's behavior), it does
            // not shift the ones after it.
            let decoded = (|| {
                let cell = iterated;
                let raw = cell.raw_cell().ok()?;
                if raw.codepoint().ok() != Some(KITTY_PLACEHOLDER) {
                    return None;
                }
                let style = cell.style().ok()?;
                let len = cell.graphemes_len().ok()?;
                graphemes.resize(len, '\0');
                cell.graphemes_buf(&mut graphemes).ok()?;
                let mark = |slot: usize| {
                    graphemes
                        .get(1 + slot)
                        .and_then(|&ch| diacritic_index(u32::from(ch)))
                };
                Some(RunCell {
                    image_id_low: color_to_id(style.fg_color),
                    image_id_high: mark(2)
                        .and_then(|high| u8::try_from(high).ok())
                        .map(u32::from),
                    placement_id: color_to_id(style.underline_color),
                    frag_row: mark(0),
                    frag_col: mark(1),
                })
            })();
            match (decoded, partial.take()) {
                // Continuing a run
                (Some(next), Some(prev)) if runs_match(&prev, &next) => {
                    partial = Some(RunPartial {
                        width: prev.width + 1,
                        ..prev
                    });
                }
                // A different or first run: the previous one ends here
                (None, prev) => {
                    if let Some(prev) = prev {
                        emit_run(prev, virtuals, fallback, cell, &mut below, &mut above);
                    }
                }
                // A run that does not continue: emit it and start a new one
                (Some(next), prev) => {
                    if let Some(prev) = prev {
                        emit_run(prev, virtuals, fallback, cell, &mut below, &mut above);
                    }
                    let fragment = (next.frag_row.unwrap_or(0), next.frag_col.unwrap_or(0));
                    partial = Some(RunPartial {
                        start_col: this_col,
                        start_row: this_row,
                        cell: next,
                        fragment,
                        width: 1,
                    });
                }
            }
        }
        // A run never spans rows: it ends at the last cell of its row
        if let Some(prev) = partial.take() {
            emit_run(prev, virtuals, fallback, cell, &mut below, &mut above);
        }
    }
    (below, above)
}

/// A kitty image texture on the GPU. The bind group keeps the view (and the view the texture)
/// alive.
struct CachedImage {
    bind_group: wgpu::BindGroup,
}

fn upload_image_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    upload: &ImageUpload,
) -> CachedImage {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("kitty image"),
        size: wgpu::Extent3d {
            width: upload.width,
            height: upload.height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
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
        &upload.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(upload.width * 4),
            rows_per_image: Some(upload.height),
        },
        wgpu::Extent3d {
            width: upload.width,
            height: upload.height,
            depth_or_array_layers: 1,
        },
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("kitty image"),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(sampler),
            },
        ],
    });
    CachedImage { bind_group }
}

/// Draws one z layer of image instances, grouped into consecutive runs that share a texture so
/// each run is a single instanced draw call with its own bind group
#[allow(clippy::too_many_arguments)]
fn draw_image_range(
    pass: &mut wgpu::RenderPass<'_>,
    pipeline: &wgpu::RenderPipeline,
    globals: &wgpu::BindGroup,
    buffer: &wgpu::Buffer,
    textures: &HashMap<(u32, u64), CachedImage>,
    keys: &[(u32, u64)],
    range: Range<u32>,
    bytes: u64,
) {
    let mut start = range.start;
    while start < range.end {
        let key = keys[start as usize];
        let mut end = start + 1;
        while end < range.end && keys[end as usize] == key {
            end += 1;
        }
        if let Some(cached) = textures.get(&key) {
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, globals, &[]);
            pass.set_bind_group(1, &cached.bind_group, &[]);
            pass.set_vertex_buffer(0, buffer.slice(..bytes));
            pass.draw(0..4, start..end);
        }
        start = end;
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
    /// Grid the terminal is currently sized at (columns, rows), for clipping image placements
    pub grid: (u16, u16),
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
    image_pipeline: wgpu::RenderPipeline,
    image_bind_group_layout: wgpu::BindGroupLayout,
    image_sampler: wgpu::Sampler,
    image_buffer: GrowBuffer,
    /// GPU textures for kitty images, mirroring the ledger's keys exactly
    textures: HashMap<(u32, u64), CachedImage>,
    ledger: TextureLedger,
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
    placement_iter: graphics::PlacementIterator<'static>,
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
        let image_attributes = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32x2
        ];
        let image_pipeline = create_pipeline(
            &device,
            &shader,
            config.format,
            ("vs_image", "fs_image"),
            wgpu::VertexBufferLayout {
                array_stride: size_of::<ImageInstance>() as u64,
                step_mode: wgpu::VertexStepMode::Instance,
                attributes: &image_attributes,
            },
        );
        let image_bind_group_layout = image_pipeline.get_bind_group_layout(1);
        let image_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("kitty image"),
            // Images render 1:1 on pixels: no filtering, pixel art stays pixelated
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

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
            image_buffer: GrowBuffer::new(&device, "images"),
            textures: HashMap::new(),
            ledger: TextureLedger::new(IMAGE_CACHE_BYTES, IMAGE_CACHE_KEEP_FRAMES),
            padding_logical: settings.padding,
            padding: padding_for(settings.padding, scale_factor),
            selection: SelectionColors::from(settings),
            panes: PaneStyle::from(settings),
            frame: Frame::default(),
            row_iter: RowIterator::new().map_err(|e| e.to_string())?,
            cell_iter: CellIterator::new().map_err(|e| e.to_string())?,
            placement_iter: graphics::PlacementIterator::new().map_err(|e| e.to_string())?,
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
            image_pipeline,
            image_bind_group_layout,
            image_sampler,
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
            images: PaneImages,
        }

        self.ledger.advance_frame();
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
            // Kitty images of this pane: geometry is recomputed each frame (scroll moves
            // placements), textures are uploaded only when a new (id, generation) appears
            let images = {
                let cell = (self.cache.cell_width, self.cache.cell_height);
                match view.terminal.kitty_graphics() {
                    Ok(graphics) => collect_images(
                        view.terminal,
                        &graphics,
                        &mut self.placement_iter,
                        &mut self.ledger,
                        cell,
                        view.grid,
                    ),
                    Err(_) => PaneImages::default(),
                }
            };
            for key in &images.evicted {
                self.textures.remove(key);
            }
            for draw in images.below.iter().chain(images.above.iter()) {
                if let Some(upload) = &draw.upload {
                    let cached = upload_image_texture(
                        &self.device,
                        &self.queue,
                        &self.image_bind_group_layout,
                        &self.image_sampler,
                        upload,
                    );
                    self.textures.insert(draw.key, cached);
                }
            }
            // Images referenced only by virtual (placeholder) placements: their quads are built
            // later, from the cells; their textures are needed up front here
            for (key, upload) in &images.uploads {
                if !self.textures.contains_key(key) {
                    let cached = upload_image_texture(
                        &self.device,
                        &self.queue,
                        &self.image_bind_group_layout,
                        &self.image_sampler,
                        upload,
                    );
                    self.textures.insert(*key, cached);
                }
            }
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
                images,
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
                    images_below: 0..0,
                    glyphs: start.1..end.1,
                    overlays: start.2..end.2,
                    images_above: 0..0,
                });
            }
            for pane in prepared.iter_mut() {
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
                let origin = (pane.rect.x + self.padding, pane.rect.y + self.padding);
                build_frame(
                    &pane.snapshot,
                    &mut self.row_iter,
                    &mut self.cell_iter,
                    &mut self.cache,
                    &mut self.frame,
                    &pane.colors,
                    pane.cursor,
                    self.selection,
                    origin,
                    // Cells are not merged with a floating pane's opaque background
                    start.0 as usize + usize::from(pane.floating),
                    pane.dim,
                );
                if !pane.images.virtuals.is_empty() {
                    let cell = (self.cache.cell_width, self.cache.cell_height);
                    let (runs_below, runs_above) = build_image_runs(
                        &pane.snapshot,
                        &mut self.row_iter,
                        &mut self.cell_iter,
                        &pane.images.virtuals,
                        &pane.images.fallback,
                        cell,
                    );
                    pane.images.below.extend(runs_below);
                    pane.images.above.extend(runs_above);
                }
                if let Some(color) = pane.frame {
                    let (r, t) = (pane.rect, frame_thickness);
                    self.frame.overlays.extend([
                        RectInstance::new(r.x, r.y, r.w, t, color),
                        RectInstance::new(r.x, (r.y + r.h).saturating_sub(t), r.w, t, color),
                        RectInstance::new(r.x, r.y, t, r.h, color),
                        RectInstance::new((r.x + r.w).saturating_sub(t), r.y, t, r.h, color),
                    ]);
                }
                let image_start = self.frame.images.len() as u32;
                let push_images = |frame: &mut Frame, draws: &[ImageDraw]| {
                    for draw in draws {
                        let quad = &draw.quad;
                        frame.images.push(ImageInstance {
                            pos: [
                                origin.0 as f32 + quad.x as f32,
                                origin.1 as f32 + quad.y as f32,
                            ],
                            size: [quad.w as f32, quad.h as f32],
                            uv_origin: quad.uv_origin,
                            uv_size: quad.uv_size,
                        });
                        frame.image_keys.push(draw.key);
                    }
                };
                push_images(&mut self.frame, &pane.images.below);
                let below_end = self.frame.images.len() as u32;
                push_images(&mut self.frame, &pane.images.above);
                let image_end = self.frame.images.len() as u32;
                let end = self.frame.lengths();
                self.frame.batches.push(Batch {
                    scissor: pane.rect,
                    backgrounds: start.0..end.0,
                    images_below: image_start..below_end,
                    glyphs: start.1..end.1,
                    overlays: start.2..end.2,
                    images_above: below_end..image_end,
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
        let images: &[u8] = bytemuck::cast_slice(&self.frame.images);
        self.rect_buffer
            .write(&self.device, &self.queue, &[backgrounds, overlays]);
        self.glyph_buffer.write(
            &self.device,
            &self.queue,
            &[bytemuck::cast_slice(&self.frame.glyphs)],
        );
        let background_bytes = backgrounds.len() as u64;
        let overlay_bytes = overlays.len() as u64;
        let image_bytes = images.len() as u64;
        self.image_buffer
            .write(&self.device, &self.queue, &[images]);

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
                // 2. Images below the text (z < 0)
                draw_image_range(
                    &mut pass,
                    &self.image_pipeline,
                    &self.rect_bind_group,
                    &self.image_buffer.buffer,
                    &self.textures,
                    &self.frame.image_keys,
                    batch.images_below.clone(),
                    image_bytes,
                );
                // 3. Text
                if !batch.glyphs.is_empty() {
                    pass.set_pipeline(&self.glyph_pipeline);
                    pass.set_bind_group(0, &self.glyph_bind_group, &[]);
                    pass.set_vertex_buffer(0, self.glyph_buffer.buffer.slice(..));
                    pass.draw(0..4, batch.glyphs.clone());
                }
                // 4. Underlines, strikethroughs, and cursor on top of the text
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
                // 5. Images above the text (z >= 0, the iterator's Layer::AboveText)
                draw_image_range(
                    &mut pass,
                    &self.image_pipeline,
                    &self.rect_bind_group,
                    &self.image_buffer.buffer,
                    &self.textures,
                    &self.frame.image_keys,
                    batch.images_above.clone(),
                    image_bytes,
                );
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
        // Rows holding kitty unicode placeholders get their glyph text suppressed: the font has
        // no U+10EEEE and the diacritics are combining marks, so they would render as tofu on
        // top of the image slice that the cell actually shows (see `build_image_runs`)
        let row_has_placeholders = row
            .raw_row()
            .ok()
            .and_then(|raw| raw.has_kitty_virtual_placeholder().ok())
            .unwrap_or(false);
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
            let is_placeholder = row_has_placeholders
                && cell.raw_cell().ok().and_then(|raw| raw.codepoint().ok())
                    == Some(KITTY_PLACEHOLDER);
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
            if !style.invisible
                && !is_spacer
                && !is_placeholder
                && cell.graphemes_len().unwrap_or(0) > 0
            {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use libghostty_vt::kitty::graphics;
    use libghostty_vt::{Terminal, TerminalOptions};

    const CELL: (u32, u32) = (8, 16);
    const GRID: (u16, u16) = (10, 6);

    /// A placement drawn with no source rectangle (the whole texture) and the texture sized to
    /// the rendered pixels, which is what a full-image placement with no scaling looks like.
    fn quad(viewport: (i32, i32), offset: (u32, u32), pixel: (u32, u32)) -> Option<ImageQuad> {
        image_quad(
            viewport,
            offset,
            pixel,
            (0, 0, pixel.0, pixel.1),
            pixel,
            CELL,
            GRID,
        )
    }

    #[test]
    fn a_placement_maps_to_its_cell_rect() {
        // (2, 1) cells in, 32x48 rendered pixels
        let q = quad((2, 1), (0, 0), (32, 48)).unwrap();
        assert_eq!((q.x, q.y, q.w, q.h), (16, 16, 32, 48));
        assert_eq!(q.uv_origin, [0.0, 0.0]);
        assert_eq!(q.uv_size, [1.0, 1.0]);
    }

    #[test]
    fn a_placement_offsets_within_its_starting_cell() {
        let q = quad((0, 0), (3, 5), (8, 16)).unwrap();
        assert_eq!((q.x, q.y), (3, 5));
    }

    #[test]
    fn a_scrolled_up_placement_is_clipped_at_the_top_with_its_uv_shifted() {
        // 3 rows tall (48 px), origin 2 rows above the viewport: the top 2 rows are gone
        let q = quad((0, -2), (0, 0), (8, 48)).unwrap();
        assert_eq!((q.x, q.y, q.w, q.h), (0, 0, 8, 16));
        // The visible part is the bottom third of the texture
        assert!(
            (q.uv_origin[1] - 32.0 / 48.0).abs() < 1e-6,
            "{:?}",
            q.uv_origin
        );
        assert!((q.uv_size[1] - 16.0 / 48.0).abs() < 1e-6, "{:?}", q.uv_size);
        assert_eq!(q.uv_origin[0], 0.0);
        assert_eq!(q.uv_size[0], 1.0);
    }

    #[test]
    fn a_fully_scrolled_out_placement_is_not_drawn() {
        assert!(quad((0, -3), (0, 0), (8, 48)).is_none());
        // ...and nothing below the grid either
        assert!(quad((0, GRID.1 as i32), (0, 0), (8, 16)).is_none());
    }

    #[test]
    fn a_placement_is_clipped_at_the_grid_edges() {
        // The grid is 80x96 px; the quad sticks out 24 px to the right and 16 px below
        let q = quad((8, 4), (0, 0), (32, 48)).unwrap();
        assert_eq!((q.x, q.y, q.w, q.h), (64, 64, 16, 32));
        assert!((q.uv_size[0] - 16.0 / 32.0).abs() < 1e-6);
        assert!((q.uv_size[1] - 32.0 / 48.0).abs() < 1e-6);
    }

    #[test]
    fn a_source_rectangle_maps_into_uv_space() {
        let q = image_quad(
            (0, 0),
            (0, 0),
            (32, 32),
            (16, 16, 32, 32),
            (64, 64),
            CELL,
            GRID,
        )
        .unwrap();
        assert_eq!(q.uv_origin, [0.25, 0.25]);
        assert_eq!(q.uv_size, [0.5, 0.5]);
    }

    #[test]
    fn degenerate_dimensions_draw_nothing() {
        assert!(quad((0, 0), (0, 0), (0, 16)).is_none());
        assert!(quad((0, 0), (0, 0), (16, 0)).is_none());
        assert!(image_quad((0, 0), (0, 0), (8, 16), (0, 0, 8, 16), (0, 16), CELL, GRID).is_none());
    }

    #[test]
    fn a_fresh_texture_is_not_evicted_under_the_cap() {
        let mut ledger = TextureLedger::new(1000, 10);
        assert!(ledger.touch((1, 1), 400).is_empty());
        assert!(
            ledger.touch((1, 1), 400).is_empty(),
            "a second touch is a refresh"
        );
    }

    #[test]
    fn the_least_recently_used_textures_are_evicted_when_the_cap_is_exceeded() {
        let mut ledger = TextureLedger::new(300, 10);
        ledger.touch((1, 1), 100); // frame 1
        ledger.touch((2, 1), 100); // frame 1
        ledger.advance_frame();
        ledger.touch((3, 1), 100); // frame 2: cache is exactly full
        // A fourth texture of 200 bytes does not fit: the two from frame 1 go, the frame-2 one
        // stays (ties in recency break by key, so the order is deterministic)
        let evicted = ledger.touch((4, 1), 200);
        assert_eq!(evicted, vec![(1, 1), (2, 1)]);
    }

    #[test]
    fn a_texture_unused_for_too_long_is_evicted_even_under_the_cap() {
        let mut ledger = TextureLedger::new(1 << 30, 5);
        ledger.touch((1, 1), 100);
        for _ in 0..5 {
            ledger.advance_frame();
        }
        let evicted = ledger.touch((2, 1), 100);
        assert_eq!(evicted, vec![(1, 1)]);
    }

    #[test]
    fn a_replaced_generation_becomes_a_new_entry_and_the_old_one_is_evictable() {
        let mut ledger = TextureLedger::new(150, 10);
        ledger.touch((1, 7), 100);
        ledger.advance_frame();
        // Same image id, new generation (a retransmission): 200 bytes do not fit with the old one
        let evicted = ledger.touch((1, 8), 100);
        assert_eq!(evicted, vec![(1, 7)]);
        assert!(ledger.is_cached((1, 8)));
    }

    #[test]
    fn remove_frees_the_bytes_for_a_texture_that_could_not_be_uploaded() {
        let mut ledger = TextureLedger::new(150, 10);
        ledger.touch((1, 1), 100);
        ledger.remove((1, 1));
        assert!(!ledger.is_cached((1, 1)));
        assert!(
            ledger.touch((2, 1), 100).is_empty(),
            "nothing evicted: the cache is empty"
        );
    }

    #[test]
    fn the_key_just_touched_is_never_evicted() {
        let mut ledger = TextureLedger::new(100, 10);
        ledger.touch((1, 1), 60);
        let evicted = ledger.touch((2, 1), 60);
        assert_eq!(evicted, vec![(1, 1)]);
        assert!(ledger.is_cached((2, 1)));
    }

    #[test]
    fn rgba_passes_through_unchanged() {
        let pixels = image_pixels(
            ImageFormat::Rgba,
            Compression::None,
            2,
            1,
            &[1, 2, 3, 4, 5, 6, 7, 8],
        );
        assert_eq!(pixels.unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn rgb_is_expanded_to_opaque_rgba() {
        let pixels = image_pixels(
            ImageFormat::Rgb,
            Compression::None,
            2,
            1,
            &[1, 2, 3, 4, 5, 6],
        );
        assert_eq!(pixels.unwrap(), vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn gray_and_gray_alpha_expand_to_rgba() {
        let gray = image_pixels(ImageFormat::Gray, Compression::None, 1, 1, &[7]);
        assert_eq!(gray.unwrap(), vec![7, 7, 7, 255]);
        let gray_alpha = image_pixels(ImageFormat::GrayAlpha, Compression::None, 1, 1, &[9, 40]);
        assert_eq!(gray_alpha.unwrap(), vec![9, 9, 9, 40]);
    }

    #[test]
    fn truncated_data_is_rejected() {
        assert!(image_pixels(ImageFormat::Rgba, Compression::None, 2, 1, &[1, 2, 3]).is_none());
    }

    #[test]
    fn formats_the_pipeline_cannot_render_are_rejected() {
        // A stored PNG would have been decoded on transmit already: if one is ever stored as-is,
        // its bytes are not RGBA
        assert!(image_pixels(ImageFormat::Png, Compression::None, 1, 1, &[1, 2, 3, 4]).is_none());
        // zlib-deflated raw transmits are not decompressed here (PNG, f=100, is the compressed
        // path in use, and it is decoded on transmit)
        assert!(
            image_pixels(
                ImageFormat::Rgba,
                Compression::ZlibDeflate,
                1,
                1,
                &[1, 2, 3, 4]
            )
            .is_none()
        );
    }

    /// The exact payload libghostty-vt's documentation uses: a 1x1 opaque red PNG
    const RED_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    #[test]
    fn a_transmitted_image_becomes_one_quad_above_the_text_and_its_texture_is_uploaded_once() {
        crate::pane::install_png_decoder();
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 10,
            rows: 6,
            max_scrollback: 0,
        })
        .unwrap();
        crate::pane::enable_graphics(&mut terminal, &Config::default());
        terminal.resize(10, 6, CELL.0, CELL.1).unwrap();
        terminal.vt_write(format!("\x1b_Ga=T,f=100,q=1;{RED_PNG_B64}\x1b\\").as_bytes());

        let graphics = terminal.kitty_graphics().unwrap();
        let mut iter = graphics::PlacementIterator::new().unwrap();
        let mut ledger = TextureLedger::new(u64::MAX, u64::MAX);
        let images = collect_images(&terminal, &graphics, &mut iter, &mut ledger, CELL, GRID);

        assert!(images.below.is_empty());
        assert_eq!(
            images.above.len(),
            1,
            "z=0 is above the text, per the iterator's Layer"
        );
        assert!(images.evicted.is_empty());
        let draw = &images.above[0];
        // A transmit with no explicit i= gets kitty's internal auto id, not 1: the key must
        // point at the image that is actually stored
        assert!(
            graphics.image(draw.key.0).is_some(),
            "the key names a stored image"
        );
        assert!(draw.key.1 != 0, "stored images never have generation 0");
        // A 1x1 image with no cols/rows is displayed at its native size: one pixel
        assert_eq!(
            (draw.quad.x, draw.quad.y, draw.quad.w, draw.quad.h),
            (0, 0, 1, 1)
        );
        assert_eq!(draw.quad.uv_origin, [0.0, 0.0]);
        assert_eq!(draw.quad.uv_size, [1.0, 1.0]);
        let upload = draw.upload.as_ref().unwrap();
        assert_eq!((upload.width, upload.height), (1, 1));
        assert_eq!(upload.rgba, vec![255, 0, 0, 255]);

        // The same content the next frame: the texture stays cached, nothing to upload
        let images = collect_images(&terminal, &graphics, &mut iter, &mut ledger, CELL, GRID);
        assert!(images.above[0].upload.is_none(), "already cached");
    }

    #[test]
    fn diacritic_indices_match_kitty() {
        assert_eq!(diacritic_index(0x0305), Some(0));
        assert_eq!(diacritic_index(0x030D), Some(1));
        assert_eq!(diacritic_index(0x030E), Some(2));
        assert_eq!(diacritic_index(0x0310), Some(3));
        assert_eq!(diacritic_index(0x0483), Some(30));
        assert_eq!(diacritic_index(0x1D242), Some(294));
        assert_eq!(diacritic_index(0x0327), None, "not one of kitty's marks");
    }

    #[test]
    fn style_colors_encode_kitty_image_ids() {
        use libghostty_vt::style::PaletteIndex;
        assert_eq!(
            color_to_id(StyleColor::Rgb(RgbColor { r: 1, g: 2, b: 3 })),
            (1 << 16) | (2 << 8) | 3
        );
        assert_eq!(color_to_id(StyleColor::Palette(PaletteIndex(42))), 42);
        assert_eq!(color_to_id(StyleColor::None), 0);
    }

    /// libghostty's own fixtures ("dog", 500x306 pixels, cell 36x80, placement c=4 r=2):
    /// the expected values are transcribed from its tests in `graphics_unicode.zig`
    fn dog(col: u32, row: u32, width: u32) -> VirtualRect {
        virtual_render(4, 2, col, row, width, (500, 306), (36, 80)).unwrap()
    }

    #[test]
    fn a_virtual_run_matches_libghostty_dog_4x2() {
        assert_eq!(
            dog(0, 0, 4),
            VirtualRect {
                offset_x: 0,
                offset_y: 36,
                dest_w: 144,
                dest_h: 44,
                src_x: 0,
                src_y: 0,
                src_w: 500,
                src_h: 153
            }
        );
        assert_eq!(
            dog(0, 1, 4),
            VirtualRect {
                offset_x: 0,
                offset_y: 0,
                dest_w: 144,
                dest_h: 44,
                src_x: 0,
                src_y: 153,
                src_w: 500,
                src_h: 153
            }
        );
    }

    #[test]
    fn a_virtual_run_matches_libghostty_dog_2x2() {
        let rect = virtual_render(2, 2, 0, 0, 4, (500, 306), (36, 80)).unwrap();
        assert_eq!(
            rect,
            VirtualRect {
                offset_x: 0,
                offset_y: 58,
                dest_w: 72,
                dest_h: 22,
                src_x: 0,
                src_y: 0,
                src_w: 500,
                src_h: 153
            }
        );
    }

    #[test]
    fn a_virtual_run_matches_libghostty_dog_1x1() {
        let rect = virtual_render(1, 1, 0, 0, 4, (500, 306), (36, 80)).unwrap();
        assert_eq!(
            rect,
            VirtualRect {
                offset_x: 0,
                offset_y: 29,
                dest_w: 36,
                dest_h: 22,
                src_x: 0,
                src_y: 0,
                src_w: 500,
                src_h: 306
            }
        );
    }

    #[test]
    fn an_unsized_virtual_placement_fits_the_whole_image_in_the_grid() {
        // No c/r: the grid auto-sizes to hold the image at its original size
        let rect = virtual_render(0, 0, 0, 0, 1, (17, 33), (8, 16)).unwrap();
        assert_eq!(rect.dest_w, 8, "the run is one cell wide");
    }

    #[test]
    fn a_slice_fully_inside_a_blank_band_draws_nothing() {
        // An extremely wide panorama in a 4-row grid: the top row of cells lies entirely in the
        // centered image's blank band, so it has no pixels to show
        assert!(virtual_render(4, 4, 0, 0, 4, (10000, 10), (8, 16)).is_none());
    }

    /// A 4x4 PNG whose top half is one gray and its bottom half another, so a slice's vertical
    /// uv window says which part of the image it samples
    fn two_tone_png() -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut data = [0u8; 4 * 4 * 4];
        for (index, px) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let gray = if index < 8 { 10 } else { 200 }; // top half vs bottom half
            px.copy_from_slice(&[gray, gray, gray, 255]);
        }
        {
            let mut enc = png::Encoder::new(&mut encoded, 4, 4);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(&data).unwrap();
        }
        encoded
    }

    #[test]
    fn a_unicode_placeholder_run_renders_per_cell_slices() {
        use base64::Engine as _;
        crate::pane::install_png_decoder();
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 16,
            rows: 6,
            max_scrollback: 0,
        })
        .unwrap();
        crate::pane::enable_graphics(&mut terminal, &Config::default());
        terminal.resize(16, 6, 8, 16).unwrap();

        // Transmit with an explicit id (placeholder cells quote it through the fg color)
        let b64 = base64::engine::general_purpose::STANDARD.encode(two_tone_png());
        terminal.vt_write(format!("\x1b_Ga=t,f=100,i=5,q=1;{b64}\x1b\\").as_bytes());
        // The virtual placement: a 4x2-cell grid referenced by unicode placeholders
        terminal.vt_write("\x1b_Ga=p,i=5,q=1,U=1,c=4,r=2\x1b\\".as_bytes());

        // The placeholder cells: two lines, each 4 consecutive cells naming fragments
        // (0,0).(0,1).(0,2).(0,3) / (1,0).(1,1).(1,2).(1,3), image id 5 via SGR 38;5;5
        let mark = |index: u32| char::from_u32(*DIACRITICS.get(index as usize).unwrap()).unwrap();
        let placeholder = '\u{10EEEE}';
        let mut cells = String::from("\x1b[?2027h\x1b[38;5;5m"); // grapheme clustering on
        for frag_row in [0u32, 1] {
            if frag_row > 0 {
                cells.push_str("\r\n");
            }
            for col in 0..4u32 {
                cells.push(placeholder);
                cells.push(mark(frag_row)); // the fragment row the run shows
                cells.push(mark(col)); // the fragment column
            }
        }
        terminal.vt_write(cells.as_bytes());

        let graphics = terminal.kitty_graphics().unwrap();
        let mut iter = graphics::PlacementIterator::new().unwrap();
        let mut ledger = TextureLedger::new(u64::MAX, u64::MAX);
        let images = collect_images(
            &terminal,
            &graphics,
            &mut iter,
            &mut ledger,
            (8, 16),
            (16, 6),
        );

        assert!(
            images.below.is_empty() && images.above.is_empty(),
            "a virtual placement yields no quad of its own"
        );
        assert_eq!(
            images.uploads.len(),
            1,
            "its texture is uploaded via the ref"
        );
        assert_eq!(images.virtuals.len(), 1, "one virtual placement");
        let reference = images.virtuals.values().next().unwrap();
        assert_eq!((reference.columns, reference.rows), (4, 2), "the a=p grid");
        assert_eq!(reference.key.0, 5, "references image 5");

        // The cells: two runs (one per grid row), each drawing its slice of the image
        let mut state = RenderState::new().unwrap();
        let snapshot = state.update(&terminal).unwrap();
        let mut rows = RowIterator::new().unwrap();
        let mut cols = CellIterator::new().unwrap();
        let (below, above) = build_image_runs(
            &snapshot,
            &mut rows,
            &mut cols,
            &images.virtuals,
            &images.fallback,
            (8, 16),
        );
        assert!(below.is_empty());
        assert_eq!(
            above.len(),
            2,
            "one run per grid row (z=0 is above the text)"
        );
        for (run, uv_origin_y) in above.iter().zip([0.0f32, 0.5]) {
            assert_eq!(run.key, reference.key);
            assert_eq!(
                (run.quad.x, run.quad.y, run.quad.w, run.quad.h),
                (0, if uv_origin_y == 0.0 { 0 } else { 16 }, 32, 16),
                "4 cells wide, one cell tall, at its grid row"
            );
            assert!(
                (run.quad.uv_origin[1] - uv_origin_y).abs() < 1e-6,
                "{:?}",
                run.quad.uv_origin
            );
            assert!(
                (run.quad.uv_size[1] - 0.5).abs() < 1e-6,
                "{:?}",
                run.quad.uv_size
            );
        }
    }

    #[test]
    fn a_virtual_placement_without_placeholder_cells_renders_nothing() {
        use base64::Engine as _;
        crate::pane::install_png_decoder();
        let mut terminal: Terminal<'static, 'static> = Terminal::new(TerminalOptions {
            cols: 16,
            rows: 6,
            max_scrollback: 0,
        })
        .unwrap();
        crate::pane::enable_graphics(&mut terminal, &Config::default());
        terminal.resize(16, 6, 8, 16).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(two_tone_png());
        terminal.vt_write(format!("\x1b_Ga=t,f=100,i=5,q=1;{b64}\x1b\\").as_bytes());
        terminal.vt_write("\x1b_Ga=p,i=5,q=1,U=1,c=4,r=2\x1b\\".as_bytes());

        let graphics = terminal.kitty_graphics().unwrap();
        let mut iter = graphics::PlacementIterator::new().unwrap();
        let mut ledger = TextureLedger::new(u64::MAX, u64::MAX);
        let images = collect_images(
            &terminal,
            &graphics,
            &mut iter,
            &mut ledger,
            (8, 16),
            (16, 6),
        );
        assert_eq!(images.virtuals.len(), 1);

        let mut state = RenderState::new().unwrap();
        let snapshot = state.update(&terminal).unwrap();
        let mut rows = RowIterator::new().unwrap();
        let mut cols = CellIterator::new().unwrap();
        let (below, above) = build_image_runs(
            &snapshot,
            &mut rows,
            &mut cols,
            &images.virtuals,
            &images.fallback,
            (8, 16),
        );
        assert!(below.is_empty() && above.is_empty(), "no cells, no runs");
    }
}
