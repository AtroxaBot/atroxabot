//! Pictures: reading the formats the app shows and sends (PNG, JPEG, WebP),
//! and making one that is too large for a model small enough.
//!
//! A model is shown a picture of at most [`MAX_SIDE`] pixels a side and
//! [`attach::MAX_IMAGE`] bytes: Anthropic scales anything larger down
//! itself, refuses pictures over 2000 pixels once a request carries more
//! than twenty, and takes no more than 5 MB of base64. A larger one is
//! scaled to fit and written again ([`shrink`]): as a PNG when it was one
//! (a screenshot stays sharp), as a JPEG otherwise or when the PNG is still
//! too heavy. [`sendable`] does that for a file being attached to a message,
//! once, into a folder of the app's own: every engine then reads the
//! small copy, and reads the same bytes every time.
//!
//! Decoding is left to fuzzed crates (a file is hostile input); the JPEG
//! is written here, baseline and unsubsampled, from standard tables.

use std::path::{Path, PathBuf};

use crate::attach::{self, MAX_IMAGE};

/// Largest image decoded, in pixels (about 190 MB of RGBA).
pub const MAX_PIXELS: usize = 48_000_000;
/// The longest side of a picture a model is shown, in pixels.
pub const MAX_SIDE: u32 = 2000;
/// Largest file read to make a smaller copy of.
const MAX_FILE: u64 = 64 << 20;
/// The quality the JPEG of a smaller copy is written at (of 100).
const QUALITY: u8 = 85;

/// Straight-alpha RGBA pixels, with their width and height.
pub type Pixels = (Vec<u8>, u32, u32);

/// The size of an image of `size` made as large as fits `within`, its
/// proportions kept, and never larger than it is.
#[must_use]
pub fn fit(size: (u32, u32), within: (u32, u32)) -> (u32, u32) {
    let scale = (f64::from(within.0) / f64::from(size.0.max(1))).min(f64::from(within.1) / f64::from(size.1.max(1)));
    let scale = scale.min(1.0);
    let scaled = |length: u32, most: u32| ((f64::from(length) * scale).round() as u32).clamp(1, most.max(1));
    (scaled(size.0, within.0), scaled(size.1, within.1))
}

/// Decodes a PNG, JPEG or WebP to straight-alpha RGBA, with its EXIF
/// orientation (1 when absent; only JPEG's is read).
///
/// # Errors
/// A human-readable reason: too large, damaged, or none of the three.
pub fn decode(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32, u8), String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let (rgba, w, h) = decode_png(bytes)?;
        Ok((rgba, w, h, 1))
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(bytes)
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        let (rgba, w, h) = decode_webp(bytes)?;
        Ok((rgba, w, h, 1))
    } else {
        Err("not a PNG, JPEG or WebP image".into())
    }
}

/// The width and height a PNG, JPEG or WebP says it has, from its header
/// alone (before any EXIF orientation); `None` for anything else.
#[must_use]
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        // The header chunk comes first: its length and name, then the width and the height.
        let field = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        (bytes.get(12..16) == Some(b"IHDR")).then(|| field(16).zip(field(20))).flatten()
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        let mut decoder = zune_jpeg::JpegDecoder::new(zune_jpeg::zune_core::bytestream::ZCursor::new(bytes));
        decoder.decode_headers().ok()?;
        decoder.info().map(|info| (u32::from(info.width), u32::from(info.height)))
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).ok().map(|decoder| decoder.dimensions())
    } else {
        None
    }
}

/// Decodes a WebP (the first frame, if animated) to straight-alpha RGBA.
fn decode_webp(bytes: &[u8]) -> Result<Pixels, String> {
    let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    decoder.set_memory_limit(MAX_PIXELS * 4);
    let (w, h) = decoder.dimensions();
    // Checked before allocating: a WebP header may claim 16383² pixels.
    if w as usize * h as usize > MAX_PIXELS {
        return Err("the image is too large".into());
    }
    let mut buf = vec![0; decoder.output_buffer_size().ok_or("the image is too large")?];
    decoder.read_image(&mut buf).map_err(|e| e.to_string())?;
    if decoder.has_alpha() {
        return Ok((buf, w, h));
    }
    Ok((buf.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect(), w, h))
}

/// Decodes a PNG to straight-alpha RGBA.
///
/// # Errors
/// A human-readable reason: too large, or damaged.
pub fn decode_png(bytes: &[u8]) -> Result<Pixels, String> {
    let mut decoder = png::Decoder::new_with_limits(std::io::Cursor::new(bytes), png::Limits { bytes: MAX_PIXELS * 4 });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let size = reader.output_buffer_size().ok_or("the image is too large")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width, info.height);
    if w as usize * h as usize > MAX_PIXELS {
        return Err("the image is too large".into());
    }
    let rows = buf.chunks_exact(info.line_size).take(h as usize);
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for row in rows {
        let row = &row[..(w as usize * info.color_type.samples()).min(row.len())];
        match info.color_type {
            png::ColorType::Rgba => rgba.extend_from_slice(row),
            png::ColorType::Rgb => {
                row.as_chunks::<3>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255]));
            }
            png::ColorType::GrayscaleAlpha => {
                row.as_chunks::<2>().0.iter().for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]]));
            }
            png::ColorType::Grayscale => row.iter().for_each(|&g| rgba.extend_from_slice(&[g, g, g, 255])),
            png::ColorType::Indexed => return Err("unexpanded palette image".into()),
        }
    }
    if rgba.len() != w as usize * h as usize * 4 {
        return Err("truncated image data".into());
    }
    Ok((rgba, w, h))
}

/// Decodes a JPEG to RGBA, with its EXIF orientation (1 when absent).
fn decode_jpeg(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32, u8), String> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let side = (MAX_PIXELS as f64).sqrt() as usize * 2;
    let options =
        DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA).set_max_width(side).set_max_height(side);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), options);
    decoder.decode_headers().map_err(|e| e.to_string())?;
    let info = decoder.info().ok_or("missing image header")?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if w as usize * h as usize > MAX_PIXELS {
        return Err("the image is too large".into());
    }
    let orientation = decoder.exif().map_or(1, |exif| exif_orientation(exif));
    let len = w as usize * h as usize * 4;
    let mut rgba = vec![0; decoder.output_buffer_size().ok_or("missing image header")?.max(len)];
    decoder.decode_into(&mut rgba).map_err(|e| e.to_string())?;
    rgba.truncate(len);
    Ok((rgba, w, h, orientation))
}

/// The orientation tag (1–8) of EXIF data starting at its TIFF header; 1 if
/// absent or malformed.
#[must_use]
pub fn exif_orientation(exif: &[u8]) -> u8 {
    let big = match exif.get(..4) {
        Some(b"MM\0*") => true,
        Some(b"II*\0") => false,
        _ => return 1,
    };
    let u16_at = |at: usize| {
        exif.get(at..at + 2)
            .map(|b| if big { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) })
    };
    let u32_at = |at: usize| {
        exif.get(at..at + 4).map(|b| {
            if big {
                u32::from_be_bytes([b[0], b[1], b[2], b[3]])
            } else {
                u32::from_le_bytes([b[0], b[1], b[2], b[3]])
            }
        })
    };
    let Some(ifd) = u32_at(4).map(|o| o as usize) else {
        return 1;
    };
    let count = u16_at(ifd).unwrap_or(0);
    for i in 0..usize::from(count) {
        let entry = ifd + 2 + i * 12;
        if u16_at(entry) == Some(0x0112) {
            return u16_at(entry + 8).filter(|o| (1..=8).contains(o)).map_or(1, |o| o as u8);
        }
    }
    1
}

/// Scales `src` (`sw`×`sh` RGBA) to cover `dw`×`dh`, cropping the overflow
/// evenly, by averaging every source pixel under each target pixel (with
/// alpha weighting, so transparent edges do not darken).
#[must_use]
pub fn cover(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0; dw as usize * dh as usize * 4];
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return out;
    }
    // Source pixels per target pixel, the same on both axes.
    let step = f64::from(sw) / f64::from(dw);
    let step = step.min(f64::from(sh) / f64::from(dh));
    let x0 = (f64::from(sw) - step * f64::from(dw)) * 0.5;
    let y0 = (f64::from(sh) - step * f64::from(dh)) * 0.5;
    let span = |start: f64, i: u32, limit: u32| {
        let a = ((start + step * f64::from(i)) as u32).min(limit - 1);
        let b = ((start + step * f64::from(i + 1)) as u32).clamp(a + 1, limit);
        a..b
    };
    for ty in 0..dh {
        let rows = span(y0, ty, sh);
        for tx in 0..dw {
            let cols = span(x0, tx, sw);
            let mut sum = [0u64; 4];
            for y in rows.clone() {
                let row = y as usize * sw as usize;
                for x in cols.clone() {
                    let p = &src[(row + x as usize) * 4..][..4];
                    let a = u64::from(p[3]);
                    sum[0] += u64::from(p[0]) * a;
                    sum[1] += u64::from(p[1]) * a;
                    sum[2] += u64::from(p[2]) * a;
                    sum[3] += a;
                }
            }
            let n = u64::from(rows.end - rows.start) * u64::from(cols.end - cols.start);
            let o = &mut out[(ty as usize * dw as usize + tx as usize) * 4..][..4];
            for c in 0..3 {
                if let Some(v) = sum[c].checked_div(sum[3]) {
                    o[c] = v as u8;
                }
            }
            o[3] = (sum[3] / n) as u8;
        }
    }
    out
}

/// Applies EXIF `orientation` to `src` (`w`×`h` RGBA). Orientations 5–8
/// return an `h`×`w` image.
#[must_use]
pub fn orient(src: &[u8], w: u32, h: u32, orientation: u8) -> Vec<u8> {
    if !(2..=8).contains(&orientation) {
        return src.to_vec();
    }
    let (w, h) = (w as usize, h as usize);
    let swap = orientation >= 5;
    let out_w = if swap { h } else { w };
    let mut out = vec![0; src.len()];
    for y in 0..h {
        for x in 0..w {
            // Where source pixel (x, y) lands in the output.
            let (ox, oy) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (h - 1 - y, x),
                7 => (h - 1 - y, w - 1 - x),
                _ => (y, w - 1 - x),
            };
            let from = (y * w + x) * 4;
            let to = (oy * out_w + ox) * 4;
            out[to..to + 4].copy_from_slice(&src[from..from + 4]);
        }
    }
    out
}

/// Whether a picture of `bytes` whose header says `size` is more than a
/// model is shown: too many pixels a side, or too many bytes.
fn oversized(bytes: u64, size: (u32, u32)) -> bool {
    bytes > MAX_IMAGE || size.0.max(size.1) > MAX_SIDE
}

/// A copy of the picture `bytes` (a PNG, JPEG or WebP) that a model is
/// shown: at most [`MAX_SIDE`] pixels a side and [`MAX_IMAGE`] bytes, the
/// right way up, with its MIME type. `None` when the picture is fine as it
/// is, or cannot be read or made to fit.
#[must_use]
pub fn shrink(bytes: &[u8]) -> Option<(Vec<u8>, &'static str)> {
    let size = dimensions(bytes)?;
    if !oversized(bytes.len() as u64, size) {
        return None;
    }
    let was_png = bytes.starts_with(b"\x89PNG");
    let (rgba, w, h, orientation) = decode(bytes).ok()?;
    let (tw, th) = fit((w, h), (MAX_SIDE, MAX_SIDE));
    let small = if (tw, th) == (w, h) { rgba } else { cover(&rgba, w, h, tw, th) };
    let upright = orient(&small, tw, th, orientation);
    // Orientations 5–8 swap the axes.
    let (w, h) = if orientation >= 5 { (th, tw) } else { (tw, th) };
    // A PNG stays one while that is light enough: what was drawn sharp (a screenshot) stays sharp.
    if was_png && let Some(png) = encode_png(&upright, w, h).filter(|png| png.len() as u64 <= MAX_IMAGE) {
        return Some((png, "image/png"));
    }
    Some((encode_jpeg(&upright, w, h, QUALITY), "image/jpeg")).filter(|(jpeg, _)| jpeg.len() as u64 <= MAX_IMAGE)
}

/// The file to send for the picture at `path`: itself when a model can be
/// shown it as it is (or it is no picture this module reads), else a
/// smaller copy written into the folder `into`, made if need be. A picture
/// that cannot be made smaller is left as it is, to be named by its path.
#[must_use]
pub fn sendable(path: &Path, into: &Path) -> PathBuf {
    let smaller = || -> Option<PathBuf> {
        attach::media_type(&path.to_string_lossy()).filter(|kind| *kind != "image/gif")?;
        let length = std::fs::metadata(path).ok().filter(std::fs::Metadata::is_file)?.len();
        if length > MAX_FILE {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        let (copy, kind) = shrink(&bytes)?;
        let extension = if kind == "image/png" { "png" } else { "jpg" };
        std::fs::create_dir_all(into).ok()?;
        let made = into.join(format!("{}.{extension}", crate::new_id()));
        std::fs::write(&made, copy).ok()?;
        Some(made)
    };
    smaller().unwrap_or_else(|| path.to_path_buf())
}

/// `rgba` (`w`×`h`) as a PNG: without its alpha when every pixel is opaque.
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let opaque = rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255);
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, w, h);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_color(if opaque { png::ColorType::Rgb } else { png::ColorType::Rgba });
    let mut writer = encoder.write_header().ok()?;
    if opaque {
        let rgb: Vec<u8> = rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        writer.write_image_data(&rgb).ok()?;
    } else {
        writer.write_image_data(rgba).ok()?;
    }
    writer.finish().ok()?;
    Some(out)
}

/// The order a block's coefficients are written in: from the lowest frequency up.
const ZIGZAG: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21,
    28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54,
    47, 55, 62, 63,
];

/// The JPEG standard's quantisation tables (its annex K) for brightness and for colour, row by row.
const QUANT: [[u8; 64]; 2] = [
    [
        16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29,
        51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121,
        120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
    ],
    [
        17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99, 47, 66, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99,
    ],
];

/// The standard's Huffman table for the first coefficient of a block (its
/// size in bits, 0 to 11): how many codes there are of each length from 1
/// to 16 bits, then the sizes in the order of their codes.
const DC_CODES: ([u8; 16], [u8; 12]) =
    ([0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);

/// The standard's Huffman table for the other coefficients: each symbol is
/// the run of zeros before one (high four bits) and its size (low four),
/// with 0x00 for "the rest is zero" and 0xF0 for sixteen zeros.
const AC_CODES: ([u8; 16], [u8; 162]) = (
    [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D],
    [
        0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71,
        0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72,
        0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37,
        0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59,
        0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83,
        0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3,
        0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3,
        0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
        0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
    ],
);

/// The code of each symbol of a Huffman table given as the standard gives
/// it: its bits (in the low end) and how many there are; 0 bits for a
/// symbol the table does not have.
fn huffman_codes(counts: &[u8; 16], symbols: &[u8]) -> [(u16, u8); 256] {
    let mut codes = [(0, 0); 256];
    let (mut code, mut next) = (0u16, 0usize);
    for (length, count) in counts.iter().enumerate() {
        for _ in 0..*count {
            if let Some(symbol) = symbols.get(next) {
                codes[usize::from(*symbol)] = (code, length as u8 + 1);
            }
            code = code.wrapping_add(1);
            next += 1;
        }
        code <<= 1;
    }
    codes
}

/// The bits of a JPEG's picture data: bytes are filled from the top, and a
/// byte of all ones is followed by a zero so that it is no marker.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    hold: u32,
    held: u8,
}

impl Bits {
    /// Appends the low `count` bits of `bits` (16 at most).
    fn put(&mut self, bits: u16, count: u8) {
        self.hold = (self.hold << count) | (u32::from(bits) & ((1 << count) - 1));
        self.held += count;
        while self.held >= 8 {
            let byte = (self.hold >> (self.held - 8)) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0);
            }
            self.held -= 8;
        }
    }

    /// A number as JPEG writes it after its size: itself when positive, else one less in that many bits.
    fn number(&mut self, value: i32, size: u8) {
        let bits = if value < 0 { value - 1 } else { value };
        self.put(bits as u16, size);
    }

    /// The bytes, the last one filled up with ones.
    fn finish(mut self) -> Vec<u8> {
        if self.held > 0 {
            self.put(0x7F, 8 - self.held);
        }
        self.out
    }
}

/// How many bits a coefficient takes.
fn size_of(value: i32) -> u8 {
    (32 - value.unsigned_abs().leading_zeros()) as u8
}

/// A block of 64 samples as its 64 frequencies (the forward DCT), in place.
fn frequencies(block: &mut [f32; 64], cosines: &[[f32; 8]; 8]) {
    let mut half = [0.0f32; 64];
    for row in 0..8 {
        for u in 0..8 {
            half[row * 8 + u] = (0..8).map(|x| block[row * 8 + x] * cosines[u][x]).sum();
        }
    }
    for column in 0..8 {
        for v in 0..8 {
            block[v * 8 + column] = (0..8).map(|y| half[y * 8 + column] * cosines[v][y]).sum();
        }
    }
}

/// The 8×8 block of `rgba` (`size` pixels) whose corner is at `corner`, as
/// three planes of 64 samples: brightness (around zero) and the two
/// colour differences. What shows through is laid on white, and past the
/// picture's edge its last row and column are repeated.
fn block_at(rgba: &[u8], size: (usize, usize), corner: (usize, usize)) -> [[f32; 64]; 3] {
    let samples: [[f32; 3]; 64] = std::array::from_fn(|at| {
        let column = (corner.0 + at % 8).min(size.0 - 1);
        let row = (corner.1 + at / 8).min(size.1 - 1);
        let start = (row * size.0 + column) * 4;
        let pixel = rgba.get(start..start + 4).unwrap_or(&[255; 4]);
        let alpha = f32::from(pixel[3]) / 255.0;
        let [red, green, blue] = [0, 1, 2].map(|c| f32::from(pixel[c]) * alpha + 255.0 * (1.0 - alpha));
        [
            0.299 * red + 0.587 * green + 0.114 * blue - 128.0,
            -0.168_736 * red - 0.331_264 * green + 0.5 * blue,
            0.5 * red - 0.418_688 * green - 0.081_312 * blue,
        ]
    });
    [0, 1, 2].map(|plane| samples.map(|sample| sample[plane]))
}

/// `rgba` (`width`×`height`, each from 1 to 65535) as a baseline JPEG at
/// `quality` (1 to 100), its transparency laid on white. Colour is kept
/// at full resolution, so coloured edges (text in a screenshot) stay clean.
#[must_use]
pub fn encode_jpeg(rgba: &[u8], width: u32, height: u32, quality: u8) -> Vec<u8> {
    let size = (width.clamp(1, 65_535) as usize, height.clamp(1, 65_535) as usize);
    let quality = u32::from(quality.clamp(1, 100));
    let scale = if quality < 50 { 5000 / quality } else { 200 - 2 * quality };
    let tables = QUANT.map(|table| table.map(|base| ((u32::from(base) * scale + 50) / 100).clamp(1, 255) as u8));
    let (dc, ac) = (huffman_codes(&DC_CODES.0, &DC_CODES.1), huffman_codes(&AC_CODES.0, &AC_CODES.1));

    let mut out = vec![0xFF, 0xD8];
    let mut segment = |marker: u8, body: &[u8]| {
        out.extend_from_slice(&[0xFF, marker]);
        out.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
        out.extend_from_slice(body);
    };
    segment(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
    for (id, table) in tables.iter().enumerate() {
        let mut body = vec![id as u8];
        body.extend(ZIGZAG.iter().map(|&at| table[at]));
        segment(0xDB, &body);
    }
    // Eight bits a sample, three components, none of them subsampled; colour uses the second table.
    let mut frame = vec![8];
    frame.extend_from_slice(&(size.1 as u16).to_be_bytes());
    frame.extend_from_slice(&(size.0 as u16).to_be_bytes());
    frame.extend_from_slice(&[3, 1, 0x11, 0, 2, 0x11, 1, 3, 0x11, 1]);
    segment(0xC0, &frame);
    for (class, (counts, symbols)) in [(0x00, (&DC_CODES.0, &DC_CODES.1[..])), (0x10, (&AC_CODES.0, &AC_CODES.1[..]))] {
        let mut body = vec![class];
        body.extend_from_slice(counts);
        body.extend_from_slice(symbols);
        segment(0xC4, &body);
    }
    // One scan with everything in it; all three components use the one pair of Huffman tables.
    segment(0xDA, &[3, 1, 0, 2, 0, 3, 0, 0, 63, 0]);

    let mut cosines = [[0.0f32; 8]; 8];
    for (frequency, row) in cosines.iter_mut().enumerate() {
        let weight = if frequency == 0 { (1.0f32 / 8.0).sqrt() } else { 0.5 };
        for (sample, cosine) in row.iter_mut().enumerate() {
            *cosine = weight * ((2 * sample + 1) as f32 * frequency as f32 * std::f32::consts::PI / 16.0).cos();
        }
    }
    let mut bits = Bits::default();
    let mut before = [0i32; 3];
    for top in (0..size.1).step_by(8) {
        for left in (0..size.0).step_by(8) {
            let mut blocks = block_at(rgba, size, (left, top));
            for (component, block) in blocks.iter_mut().enumerate() {
                frequencies(block, &cosines);
                let table = &tables[usize::from(component > 0)];
                let level = |at: usize| (block[at] / f32::from(table[at])).round() as i32;
                // The first is written as its difference from the block before, which takes eleven bits at most.
                let first = level(0).clamp(-1023, 1023);
                let step = first - before[component];
                before[component] = first;
                let size = size_of(step);
                bits.put(dc[usize::from(size)].0, dc[usize::from(size)].1);
                bits.number(step, size);
                let mut zeros = 0u8;
                for &at in &ZIGZAG[1..] {
                    // A coefficient takes ten bits at most.
                    let value = level(at).clamp(-1023, 1023);
                    if value == 0 {
                        zeros += 1;
                        continue;
                    }
                    while zeros > 15 {
                        bits.put(ac[0xF0].0, ac[0xF0].1);
                        zeros -= 16;
                    }
                    let size = size_of(value);
                    let symbol = usize::from((zeros << 4) | size);
                    bits.put(ac[symbol].0, ac[symbol].1);
                    bits.number(value, size);
                    zeros = 0;
                }
                if zeros > 0 {
                    bits.put(ac[0].0, ac[0].1);
                }
            }
        }
    }
    out.extend(bits.finish());
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(rgb).unwrap();
        out
    }

    /// A picture with soft slopes and a hard edge: `w`×`h` RGBA.
    fn scene(w: u32, h: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let edge = if x > w / 2 && y > h / 3 { 200 } else { 0 };
                rgba.extend_from_slice(&[(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, edge, 255]);
            }
        }
        rgba
    }

    #[test]
    fn decodes_the_three_formats_and_rejects_the_rest() {
        // 4×2: left half red, right half blue.
        let red_blue: Vec<u8> = (0..8).flat_map(|i| if i % 4 < 2 { [255, 0, 0] } else { [0, 0, 255] }).collect();
        let png = png_of(4, 2, &red_blue);
        let (rgba, w, h, orientation) = decode(&png).unwrap();
        assert_eq!(
            (w, h, orientation, &rgba[..4], &rgba[12..16]),
            (4, 2, 1, &[255, 0, 0, 255][..], &[0, 0, 255, 255][..])
        );
        assert_eq!(dimensions(&png), Some((4, 2)));
        let mut webp = Vec::new();
        image_webp::WebPEncoder::new(&mut webp).encode(&red_blue, 4, 2, image_webp::ColorType::Rgb8).unwrap();
        assert_eq!(decode(&webp).unwrap().0[..8], [255, 0, 0, 255, 255, 0, 0, 255]);
        assert_eq!(dimensions(&webp), Some((4, 2)));
        for broken in [
            &b"\x89PNG\r\n\x1a\ngarbage"[..],
            &[0xFF, 0xD8, 0xFF, 0x00, 0x01],
            b"GIF89a",
            b"RIFF\0\0\0\0WEBPgarbage",
            b"",
        ] {
            assert!(decode(broken).is_err(), "{broken:?}");
            assert_eq!(dimensions(broken), None, "{broken:?}");
        }
    }

    #[test]
    fn a_picture_fits_its_box_and_is_never_enlarged() {
        assert_eq!(fit((4000, 2000), (1000, 1000)), (1000, 500));
        assert_eq!(fit((2000, 4000), (1000, 1000)), (500, 1000));
        assert_eq!(fit((300, 200), (1000, 1000)), (300, 200));
        assert_eq!(fit((10_000, 1), (100, 100)), (100, 1), "a sliver keeps a pixel");
        assert_eq!(fit((0, 0), (100, 100)), (1, 1));
    }

    #[test]
    fn transparent_pixels_do_not_darken() {
        // Top row opaque white, bottom row transparent black.
        let src = [[255u8; 8], [0; 8]].concat();
        assert_eq!(cover(&src, 2, 2, 1, 1), [255, 255, 255, 127]);
    }

    #[test]
    fn exif_orientation_is_bounded_and_applied() {
        // Little-endian TIFF header, IFD at 8 with one orientation entry = 6.
        let mut exif = b"II*\0\x08\0\0\0\x01\0".to_vec();
        exif.extend_from_slice(&[0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0]);
        assert_eq!(exif_orientation(&exif), 6);
        assert_eq!(exif_orientation(&exif[..12]), 1, "truncated data is ignored");
        assert_eq!(exif_orientation(b"MM\0*\xFF\xFF\xFF\xFF"), 1);

        // A 2×1 image (A, B) rotated by 6 (90° clockwise) becomes 1×2: A over B.
        let (a, b) = ([1, 1, 1, 1], [2, 2, 2, 2]);
        let rotated = orient(&[a, b].concat(), 2, 1, 6);
        assert_eq!(rotated, [a, b].concat());
        // Orientation 8 (90° counter-clockwise): B over A.
        assert_eq!(orient(&[a, b].concat(), 2, 1, 8), [b, a].concat());
    }

    #[test]
    fn the_huffman_tables_hold_every_symbol_once() {
        // Every run of zeros (0 to 15) before every size (1 to 10), the end of a block, and sixteen zeros.
        let mut wanted: Vec<u8> = (0..16u8).flat_map(|run| (1..=10u8).map(move |size| run << 4 | size)).collect();
        wanted.extend([0x00, 0xF0]);
        wanted.sort_unstable();
        let mut held = AC_CODES.1.to_vec();
        held.sort_unstable();
        assert_eq!(held, wanted);
        for (counts, symbols) in [(&DC_CODES.0, &DC_CODES.1[..]), (&AC_CODES.0, &AC_CODES.1[..])] {
            assert_eq!(counts.iter().map(|count| usize::from(*count)).sum::<usize>(), symbols.len());
            // No code is the start of another, and none is all ones: the lengths leave room (Kraft).
            let used: u32 = counts.iter().enumerate().map(|(length, count)| u32::from(*count) << (15 - length)).sum();
            assert!(used < 1 << 16, "{used}");
            let codes = huffman_codes(counts, symbols);
            assert!(symbols.iter().all(|symbol| codes[usize::from(*symbol)].1 > 0));
        }
        let mut order = ZIGZAG.to_vec();
        order.sort_unstable();
        assert_eq!(order, (0..64).collect::<Vec<_>>());
    }

    #[test]
    fn a_written_jpeg_reads_back_as_the_picture() {
        // Sizes that are and are not whole blocks, down to a single pixel.
        for (w, h) in [(64, 48), (37, 21), (1, 1), (8, 9)] {
            let rgba = scene(w, h);
            let jpeg = encode_jpeg(&rgba, w, h, 90);
            assert_eq!(dimensions(&jpeg), Some((w, h)));
            let (back, bw, bh, orientation) = decode(&jpeg).unwrap_or_else(|e| panic!("{w}×{h}: {e}"));
            assert_eq!((bw, bh, orientation, back.len()), (w, h, 1, rgba.len()));
            let off: u64 = rgba.iter().zip(&back).map(|(a, b)| u64::from(a.abs_diff(*b))).sum();
            let mean = off as f64 / rgba.len() as f64;
            assert!(mean < 4.0, "{w}×{h}: off by {mean:.2} a sample");
        }
        // The same picture is the same bytes every time, and a lower quality is a smaller file.
        let rgba = scene(64, 48);
        assert_eq!(encode_jpeg(&rgba, 64, 48, 85), encode_jpeg(&rgba, 64, 48, 85));
        assert!(encode_jpeg(&rgba, 64, 48, 30).len() < encode_jpeg(&rgba, 64, 48, 95).len());
        // What shows through is laid on white.
        let clear = encode_jpeg(&[0, 0, 0, 0].repeat(64), 8, 8, 90);
        assert!(decode(&clear).unwrap().0.as_chunks::<4>().0.iter().all(|p| p[0] > 250 && p[1] > 250 && p[2] > 250));
    }

    #[test]
    fn a_picture_too_large_for_a_model_is_made_smaller() {
        // Fine as it is: left alone.
        let small = png_of(4, 2, &[9; 24]);
        assert!(shrink(&small).is_none());
        assert!(shrink(b"not a picture").is_none() && shrink(b"GIF89a").is_none());
        // Too wide: scaled to fit, proportions kept, still a PNG.
        let wide = scene(MAX_SIDE + 500, 50);
        let rgb: Vec<u8> = wide.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        let (copy, kind) = shrink(&png_of(MAX_SIDE + 500, 50, &rgb)).expect("made smaller");
        assert_eq!((kind, dimensions(&copy)), ("image/png", Some((MAX_SIDE, 40))));
        // A photograph (a JPEG) stays one, and comes out the right way up.
        let tall = encode_jpeg(&scene(60, MAX_SIDE + 400), 60, MAX_SIDE + 400, 90);
        let (copy, kind) = shrink(&tall).expect("made smaller");
        assert_eq!((kind, dimensions(&copy)), ("image/jpeg", Some((50, MAX_SIDE))));
        assert!(shrink(&copy).is_none(), "small enough now");
    }

    #[test]
    fn a_file_to_send_is_itself_or_a_smaller_copy() {
        let dir = std::env::temp_dir().join(format!("atroxabot-picture-{}", crate::new_id()));
        let copies = dir.join("copies");
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, bytes: &[u8]| {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        };
        let wide = scene(MAX_SIDE + 100, 10);
        let rgb: Vec<u8> = wide.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
        let large = file("large.png", &png_of(MAX_SIDE + 100, 10, &rgb));
        let made = sendable(&large, &copies);
        assert!(made.starts_with(&copies) && made.extension().is_some_and(|e| e == "png"), "{}", made.display());
        assert_eq!(dimensions(&std::fs::read(&made).unwrap()).map(|size| size.0), Some(MAX_SIDE));
        assert!(large.exists(), "the user's own file is left alone");
        // What needs no copy, is no picture, is damaged or is missing goes as it is.
        let small = file("small.png", &png_of(2, 2, &[7; 12]));
        let notes = file("notes.md", b"hello");
        let broken = file("broken.jpg", &[0xFF, 0xD8, 0xFF, 0x00]);
        for path in [small, notes, broken, dir.join("gone.png")] {
            assert_eq!(sendable(&path, &copies), path);
        }
        assert_eq!(std::fs::read_dir(&copies).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
