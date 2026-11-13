//! The application icon, drawn in code so every platform gets it at any size without image files
//! or decoders: a rounded square with a shell prompt (`❯_`) in the Kanagawa palette.
//!
//! It is also included by `build.rs` (to embed the `.ico` in the Windows executable), so it only
//! uses `std`.

/// Fraction of the canvas left empty around the rounded square (as macOS app icons do)
const MARGIN: f32 = 0.08;
/// Corner radius of the square, as a fraction of the canvas
const RADIUS: f32 = 0.2;

const BACKGROUND_TOP: [f32; 3] = rgb(0x2a2a37);
const BACKGROUND_BOTTOM: [f32; 3] = rgb(0x16161d);
const BORDER: [f32; 3] = rgb(0x54546d);
const CHEVRON: [f32; 3] = rgb(0x7e9cd8);
const CURSOR: [f32; 3] = rgb(0xe6c384);

const fn rgb(hex: u32) -> [f32; 3] {
    [
        ((hex >> 16) & 0xff) as f32 / 255.0,
        ((hex >> 8) & 0xff) as f32 / 255.0,
        (hex & 0xff) as f32 / 255.0,
    ]
}

/// Pixels of the icon at `size`×`size`, row by row from the top, as straight-alpha RGBA
pub fn rgba(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let p = [x as f32 + 0.5, y as f32 + 0.5];
            pixels.extend_from_slice(&pixel(p, s));
        }
    }
    pixels
}

fn pixel(p: [f32; 2], s: f32) -> [u8; 4] {
    let half = 0.5 * s - MARGIN * s;
    let square = rounded_rect(p, [0.5 * s, 0.5 * s], [half, half], RADIUS * s);
    let mut color = [0.0, 0.0, 0.0, 0.0];

    let t = ((p[1] / s - MARGIN) / (1.0 - 2.0 * MARGIN)).clamp(0.0, 1.0);
    let background = mix(BACKGROUND_TOP, BACKGROUND_BOTTOM, t);
    over(&mut color, background, coverage(square));
    // A thin border so the dark square does not vanish on a dark dock or taskbar
    let border = (square + 0.012 * s).abs() - 0.006 * s;
    over(&mut color, BORDER, coverage(border.max(square)) * 0.8);

    let thickness = 0.04 * s;
    let (a, b, c) = (
        [0.29 * s, 0.33 * s],
        [0.47 * s, 0.5 * s],
        [0.29 * s, 0.67 * s],
    );
    let chevron = segment(p, a, b, thickness).min(segment(p, b, c, thickness));
    over(&mut color, CHEVRON, coverage(chevron));

    let cursor = rounded_rect(p, [0.625 * s, 0.645 * s], [0.105 * s, 0.035 * s], 0.015 * s);
    over(&mut color, CURSOR, coverage(cursor));

    [
        to_byte(color[0]),
        to_byte(color[1]),
        to_byte(color[2]),
        to_byte(color[3]),
    ]
}

/// Signed distance from `p` to a rectangle with rounded corners (negative inside)
fn rounded_rect(p: [f32; 2], center: [f32; 2], half: [f32; 2], radius: f32) -> f32 {
    let qx = (p[0] - center[0]).abs() - (half[0] - radius);
    let qy = (p[1] - center[1]).abs() - (half[1] - radius);
    let outside = qx.max(0.0).hypot(qy.max(0.0));
    outside + qx.max(qy).min(0.0) - radius
}

/// Signed distance from `p` to a segment from `a` to `b` with round caps of radius `r`
fn segment(p: [f32; 2], a: [f32; 2], b: [f32; 2], r: f32) -> f32 {
    let (pa, ba) = ([p[0] - a[0], p[1] - a[1]], [b[0] - a[0], b[1] - a[1]]);
    let h = ((pa[0] * ba[0] + pa[1] * ba[1]) / (ba[0] * ba[0] + ba[1] * ba[1])).clamp(0.0, 1.0);
    (pa[0] - ba[0] * h).hypot(pa[1] - ba[1] * h) - r
}

/// How much of a pixel a shape covers, from its signed distance in pixels (antialiasing)
fn coverage(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Paints `source` with opacity `alpha` over `dest` (straight alpha)
fn over(dest: &mut [f32; 4], source: [f32; 3], alpha: f32) {
    let out = alpha + dest[3] * (1.0 - alpha);
    if out <= 0.0 {
        return;
    }
    for i in 0..3 {
        dest[i] = (source[i] * alpha + dest[i] * dest[3] * (1.0 - alpha)) / out;
    }
    dest[3] = out;
}

fn to_byte(value: f32) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The icon as a `.ico` file with one 32-bit image per size (at most 256). Windows embeds it in
/// the executable and macOS reads it with `NSImage`.
// Only the macOS binary calls it; Windows gets the `.ico` from build.rs, a separate compilation
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&size| ico_bitmap(size)).collect();
    let mut file = Vec::new();
    file.extend_from_slice(&[0, 0, 1, 0]);
    file.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&size, image) in sizes.iter().zip(&images) {
        assert!(
            (1..=256).contains(&size),
            "a .ico image is 1 to 256 pixels wide"
        );
        let side = if size == 256 { 0 } else { size as u8 };
        file.extend_from_slice(&[side, side, 0, 0]);
        file.extend_from_slice(&1u16.to_le_bytes()); // planes
        file.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        file.extend_from_slice(&(image.len() as u32).to_le_bytes());
        file.extend_from_slice(&offset.to_le_bytes());
        offset += image.len() as u32;
    }
    for image in images {
        file.extend_from_slice(&image);
    }
    file
}

/// One `.ico` image: a BMP header, the pixels as BGRA from the bottom row up, and an empty AND
/// mask (the alpha channel already says what is transparent)
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn ico_bitmap(size: u32) -> Vec<u8> {
    let pixels = rgba(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut bitmap = Vec::new();
    bitmap.extend_from_slice(&40u32.to_le_bytes());
    bitmap.extend_from_slice(&(size as i32).to_le_bytes());
    bitmap.extend_from_slice(&(2 * size as i32).to_le_bytes()); // pixels plus mask
    bitmap.extend_from_slice(&1u16.to_le_bytes());
    bitmap.extend_from_slice(&32u16.to_le_bytes());
    bitmap.extend_from_slice(&[0; 24]); // no compression, sizes and palette left at 0
    for row in (0..size as usize).rev() {
        let start = row * size as usize * 4;
        for [r, g, b, a] in pixels[start..start + size as usize * 4].as_chunks::<4>().0 {
            bitmap.extend_from_slice(&[*b, *g, *r, *a]);
        }
    }
    bitmap.resize(bitmap.len() + (mask_row * size) as usize, 0);
    bitmap
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(pixels: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        pixels[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn the_icon_is_a_rounded_square_with_transparent_corners() {
        let size = 64;
        let pixels = rgba(size);
        assert_eq!(pixels.len(), (size * size * 4) as usize);
        assert_eq!(at(&pixels, size, 0, 0)[3], 0, "outside the margin");
        assert_eq!(at(&pixels, size, 7, 7)[3], 0, "the rounded corner is cut");
        assert_eq!(at(&pixels, size, 32, 12)[3], 255, "inside the square");
    }

    #[test]
    fn the_prompt_is_drawn_in_its_colors() {
        let size = 100;
        let pixels = rgba(size);
        let chevron_tip = at(&pixels, size, 46, 50);
        assert_eq!(chevron_tip, [0x7e, 0x9c, 0xd8, 255]);
        let cursor = at(&pixels, size, 62, 64);
        assert_eq!(cursor, [0xe6, 0xc3, 0x84, 255]);
    }

    #[test]
    fn the_ico_file_indexes_every_size() {
        let sizes = [16, 32, 256];
        let file = ico(&sizes);
        assert_eq!(&file[..6], &[0, 0, 1, 0, 3, 0]);
        for (i, &size) in sizes.iter().enumerate() {
            let entry = &file[6 + 16 * i..6 + 16 * (i + 1)];
            assert_eq!(entry[0], if size == 256 { 0 } else { size as u8 });
            let length = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
            let mask = (size.div_ceil(32) * 4 * size) as usize;
            assert_eq!(length, 40 + (size * size * 4) as usize + mask);
            assert!(offset + length <= file.len());
            assert_eq!(
                u32::from_le_bytes(file[offset..offset + 4].try_into().unwrap()),
                40
            );
        }
    }
}
