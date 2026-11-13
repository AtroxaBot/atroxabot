//! Image thumbnails: decoding on worker threads and the RGBA image atlas.
//!
//! [`Painter::image`](crate::paint::Painter::image) asks the [`ImageAtlas`]
//! for a thumbnail of a file at an exact pixel size. A miss is queued; the
//! app decodes it on a worker ([`thumbnail`]) and hands the pixels back with
//! [`ImageAtlas::insert`]. Decoded pixels are not kept on the CPU: an image
//! evicted from the atlas is decoded again when it scrolls back into view,
//! so memory stays bounded by the atlas.
//!
//! A picture opened to be looked at is a thumbnail too, a large one: the
//! whole image, as large as fits a box ([`ImageKey::whole`]).
//!
//! PNG, JPEG and WebP are decoded (by `atroxabot_core::picture`): the
//! formats portraits are accepted in.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;

pub use atroxabot_core::picture::Pixels;
use atroxabot_core::picture::{cover, decode, fit, orient};

use crate::atlas::Shelves;
use crate::text::{ATLAS_SIZE, Upload, pad};

/// Largest file read for a thumbnail.
const MAX_FILE: u64 = 32 << 20;
/// Largest thumbnail side, in pixels.
const MAX_THUMB: u32 = 1024;
/// Largest whole picture, in pixels: as wide as the atlas takes, and short
/// enough to leave the thumbnails under it their room.
pub const MAX_WHOLE: (u32, u32) = (ATLAS_SIZE - 1, 1536);

/// A thumbnail: the file and the exact size it is drawn at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImageKey {
    /// Absolute path of the image file.
    pub path: String,
    /// Width in physical pixels.
    pub w: u32,
    /// Height in physical pixels.
    pub h: u32,
    /// The whole image, as large as fits `w`×`h` and no larger than it is,
    /// instead of one that covers `w`×`h` and is cropped to it.
    pub whole: bool,
}

/// What [`ImageAtlas::lookup`] found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lookup {
    /// In the atlas at this texel rectangle.
    Ready([f32; 4]),
    /// Being decoded (or queued).
    Loading,
    /// The file is not a readable image.
    Failed,
}

/// CPU side of the image atlas.
pub struct ImageAtlas {
    entries: HashMap<ImageKey, ([f32; 4], u16)>,
    shelves: Shelves,
    /// Decodes in flight, and misses not yet handed to [`ImageAtlas::take_wanted`].
    pending: HashSet<ImageKey>,
    wanted: Vec<ImageKey>,
    /// Files that did not decode; not retried.
    failed: HashSet<String>,
    /// Thumbnails not yet copied to the GPU texture.
    pub uploads: Vec<Upload>,
}

impl Default for ImageAtlas {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            shelves: Shelves::new(ATLAS_SIZE),
            pending: HashSet::new(),
            wanted: Vec::new(),
            failed: HashSet::new(),
            uploads: Vec::new(),
        }
    }
}

impl ImageAtlas {
    /// Starts a frame, for least-recently-used eviction.
    pub fn next_frame(&mut self) {
        self.shelves.next_frame();
    }

    /// Finds a thumbnail for drawing this frame, queueing a decode on a miss.
    pub fn lookup(&mut self, key: &ImageKey) -> Lookup {
        if let Some(&(uv, shelf)) = self.entries.get(key) {
            self.shelves.touch(shelf);
            return Lookup::Ready(uv);
        }
        if self.failed.contains(&key.path) {
            return Lookup::Failed;
        }
        let most = if key.whole { MAX_WHOLE } else { (MAX_THUMB, MAX_THUMB) };
        if key.w == 0 || key.h == 0 || key.w > most.0 || key.h > most.1 {
            return Lookup::Failed;
        }
        if self.pending.insert(key.clone()) {
            self.wanted.push(key.clone());
        }
        Lookup::Loading
    }

    /// Thumbnails to decode, each handed out once.
    pub fn take_wanted(&mut self) -> Vec<ImageKey> {
        std::mem::take(&mut self.wanted)
    }

    /// Stores a decoded thumbnail (`key.w`×`key.h`, or for a whole picture
    /// whatever of that it fills), or remembers that the file could not be
    /// decoded.
    pub fn insert(&mut self, key: ImageKey, pixels: Result<Pixels, String>) {
        self.pending.remove(&key);
        let (pixels, w, h) = match pixels {
            Ok((pixels, w, h)) => {
                let sized = if key.whole { w <= key.w && h <= key.h } else { (w, h) == (key.w, key.h) };
                if !sized || w == 0 || h == 0 || pixels.len() != w as usize * h as usize * 4 {
                    return;
                }
                (pixels, w, h)
            }
            Err(e) => {
                eprintln!("atroxabot: no thumbnail for {}: {e}", key.path);
                self.failed.insert(key.path);
                return;
            }
        };
        let slot = match self.shelves.alloc(w + 1, h + 1) {
            Some(slot) => slot,
            // No shelf tall enough for a whole picture is free: the atlas starts over, and
            // what else is on screen is decoded again as it is drawn.
            None if key.whole => {
                self.shelves = Shelves::new(ATLAS_SIZE);
                self.entries.clear();
                let Some(slot) = self.shelves.alloc(w + 1, h + 1) else { return };
                slot
            }
            // Everything on screen is in use; it is decoded again when drawn.
            None => return,
        };
        if let Some(shelf) = slot.evicted {
            self.entries.retain(|_, (_, s)| *s != shelf);
        }
        self.uploads.push(Upload { x: slot.x, y: slot.y, w: w + 1, h: h + 1, data: pad(&pixels, w, h, 4) });
        let uv = [slot.x as f32, slot.y as f32, w as f32, h as f32];
        self.entries.insert(key, (uv, slot.shelf));
    }
}

/// Decodes the image at `key.path` and scales it to cover `key.w`×`key.h`,
/// cropping the overflow evenly; or, for a whole picture, to fit inside
/// that, never larger than it is. Slow: run it on a worker thread.
///
/// # Errors
/// A human-readable reason: unreadable, too large, or not a PNG, JPEG or
/// WebP.
pub fn thumbnail(key: &ImageKey) -> Result<Pixels, String> {
    let mut bytes = Vec::new();
    fs::File::open(&key.path).and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut bytes)).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("the file is too large".into());
    }
    let (rgba, w, h, orientation) = decode(&bytes)?;
    // Orientations 5–8 swap the axes: scale for the transposed size.
    let swap = orientation >= 5;
    let (tw, th) = if swap { (key.h, key.w) } else { (key.w, key.h) };
    let (tw, th) = if key.whole { fit((w, h), (tw, th)) } else { (tw, th) };
    let small = cover(&rgba, w, h, tw, th);
    let upright = orient(&small, tw, th, orientation);
    Ok(if swap { (upright, th, tw) } else { (upright, tw, th) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(pixels).unwrap();
        out
    }

    #[test]
    fn decodes_crops_and_rejects() {
        let dir = std::env::temp_dir().join(format!("atroxabot-thumb-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 4×2: left half red, right half blue.
        let red_blue: Vec<u8> = (0..8).flat_map(|i| if i % 4 < 2 { [255, 0, 0] } else { [0, 0, 255] }).collect();
        let path = dir.join("a.png");
        fs::write(&path, png(4, 2, &red_blue)).unwrap();
        let key = |w, h| ImageKey { path: path.to_string_lossy().into_owned(), w, h, whole: false };

        // Covering 2×2 keeps the middle: half red, half blue.
        let (square, ..) = thumbnail(&key(2, 2)).unwrap();
        assert_eq!(&square[..4], [255, 0, 0, 255]);
        assert_eq!(&square[4..8], [0, 0, 255, 255]);
        // Upscaling repeats pixels.
        assert_eq!(thumbnail(&key(8, 4)).unwrap().0.len(), 8 * 4 * 4);
        // The whole picture is all of it: as it is where there is room, smaller where there is not.
        let whole = |w, h| thumbnail(&ImageKey { whole: true, ..key(w, h) }).unwrap();
        let (all, w, h) = whole(100, 100);
        assert_eq!((w, h, &all[..4], &all[12..16]), (4, 2, &[255, 0, 0, 255][..], &[0, 0, 255, 255][..]));
        let (half, w, h) = whole(2, 100);
        assert_eq!((w, h, &half[..]), (2, 1, &[255, 0, 0, 255, 0, 0, 255, 255][..]));

        fs::write(&path, b"\x89PNG\r\n\x1a\ngarbage").unwrap();
        assert!(thumbnail(&key(2, 2)).is_err());
        fs::write(&path, [0xFF, 0xD8, 0xFF, 0x00, 0x01]).unwrap();
        assert!(thumbnail(&key(2, 2)).is_err());
        fs::write(&path, b"GIF89a").unwrap();
        assert!(thumbnail(&key(2, 2)).is_err());
        fs::write(&path, b"RIFF\0\0\0\0WEBPgarbage").unwrap();
        assert!(thumbnail(&key(2, 2)).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn atlas_queues_each_miss_once() {
        let mut atlas = ImageAtlas::default();
        let key = ImageKey { path: "x".into(), w: 2, h: 2, whole: false };
        assert_eq!(atlas.lookup(&key), Lookup::Loading);
        assert_eq!(atlas.lookup(&key), Lookup::Loading);
        assert_eq!(atlas.take_wanted(), std::slice::from_ref(&key));
        assert!(atlas.take_wanted().is_empty());
        atlas.insert(key.clone(), Ok((vec![0; 16], 2, 2)));
        assert!(matches!(atlas.lookup(&key), Lookup::Ready(_)));
        assert_eq!(atlas.uploads.len(), 1);
        let broken = ImageKey { path: "y".into(), w: 2, h: 2, whole: false };
        atlas.insert(broken.clone(), Err("bad".into()));
        assert_eq!(atlas.lookup(&broken), Lookup::Failed);
    }

    #[test]
    fn a_whole_picture_is_stored_at_the_size_it_came_out() {
        let mut atlas = ImageAtlas::default();
        let key = ImageKey { path: "x".into(), w: 1000, h: 800, whole: true };
        assert_eq!(atlas.lookup(&key), Lookup::Loading);
        // Pixels that are not the size they say, or larger than was asked for, are dropped.
        atlas.insert(key.clone(), Ok((vec![0; 16], 3, 2)));
        atlas.insert(key.clone(), Ok((vec![0; 1001 * 4], 1001, 1)));
        assert_eq!(atlas.lookup(&key), Lookup::Loading);
        atlas.insert(key.clone(), Ok((vec![0; 300 * 200 * 4], 300, 200)));
        assert!(matches!(atlas.lookup(&key), Lookup::Ready([_, _, 300.0, 200.0])));
        // One that is larger than the atlas takes is not asked for at all.
        let huge = ImageKey { w: MAX_WHOLE.0 + 1, ..key.clone() };
        assert_eq!(atlas.lookup(&huge), Lookup::Failed);
        assert_eq!(atlas.lookup(&ImageKey { whole: false, ..key }), Lookup::Loading, "a thumbnail of its own");

        // With the atlas full of thumbnails in use, a whole picture has it started over.
        let mut full = ImageAtlas::default();
        let thumb = |index: u32| ImageKey { path: index.to_string(), w: 1023, h: 1023, whole: false };
        for index in 0..4 {
            full.insert(thumb(index), Ok((vec![0; 1023 * 1023 * 4], 1023, 1023)));
        }
        assert!(matches!(full.lookup(&thumb(0)), Lookup::Ready(_)));
        let tall = ImageKey { path: "tall".into(), w: 1200, h: 1500, whole: true };
        full.insert(tall.clone(), Ok((vec![0; 1200 * 1500 * 4], 1200, 1500)));
        assert!(matches!(full.lookup(&tall), Lookup::Ready(_)));
        assert_eq!(full.lookup(&thumb(0)), Lookup::Loading, "and the thumbnails are decoded again");
    }
}
