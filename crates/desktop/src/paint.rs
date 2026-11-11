//! Immediate-mode drawing API on top of [`Instance`]s.

use crate::gpu::Instance;
use crate::image::{ImageAtlas, ImageKey, Lookup, MAX_WHOLE};
use crate::logo::Logo;
use crate::text::{Align, Fonts, GlyphAtlas, Style, TextLayout};
use crate::theme::Palette;

/// Straight-alpha sRGB colour.
pub type Color = [f32; 4];

/// Opaque colour from `0xRRGGBB`.
#[must_use]
pub const fn hex(rgb: u32) -> Color {
    hexa(rgb, 1.0)
}

/// Colour from `0xRRGGBB` with alpha.
#[must_use]
pub const fn hexa(rgb: u32, alpha: f32) -> Color {
    [((rgb >> 16) & 0xff) as f32 / 255.0, ((rgb >> 8) & 0xff) as f32 / 255.0, (rgb & 0xff) as f32 / 255.0, alpha]
}

/// `color` with its alpha multiplied by `factor`.
#[must_use]
pub fn fade(color: Color, factor: f32) -> Color {
    [color[0], color[1], color[2], color[3] * factor]
}

/// Linear interpolation between two colours.
#[must_use]
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

/// Axis-aligned rectangle in logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub w: f32,
    /// Height.
    pub h: f32,
}

impl Rect {
    /// Creates a rectangle.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Right edge.
    #[must_use]
    pub fn right(self) -> f32 {
        self.x + self.w
    }

    /// Bottom edge.
    #[must_use]
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }

    /// Whether the point lies inside.
    #[must_use]
    pub fn contains(self, (x, y): (f32, f32)) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Overlapping area of two rectangles (empty if disjoint).
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        Self::new(
            x,
            y,
            (self.right().min(other.right()) - x).max(0.0),
            (self.bottom().min(other.bottom()) - y).max(0.0),
        )
    }
}

/// Primitive kinds understood by the shader.
const KIND_SHAPE: f32 = 0.0;
const KIND_GLYPH: f32 = 1.0;
const KIND_IMAGE: f32 = 2.0;
const KIND_GRADIENT: f32 = 3.0;

/// Records draw calls for one frame.
pub struct Painter<'a> {
    /// Fonts used for layout.
    pub fonts: &'a Fonts,
    atlas: &'a mut GlyphAtlas,
    images: &'a mut ImageAtlas,
    out: &'a mut Vec<Instance>,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    clip: Rect,
    /// Colours of the active scheme.
    pub theme: Palette,
    /// Set when the glyph atlas ran out of space mid-frame.
    pub atlas_full: bool,
}

impl<'a> Painter<'a> {
    /// Starts a frame covering `viewport`.
    pub fn new(
        fonts: &'a Fonts,
        (atlas, images): (&'a mut GlyphAtlas, &'a mut ImageAtlas),
        out: &'a mut Vec<Instance>,
        scale: f32,
        viewport: Rect,
        theme: &Palette,
    ) -> Self {
        out.clear();
        atlas.next_frame();
        images.next_frame();
        Self { fonts, atlas, images, out, scale, clip: viewport, theme: *theme, atlas_full: false }
    }

    /// Draws the image file at `path` scaled to cover `rect` (cropping the
    /// overflow) with rounded corners. Until its thumbnail is decoded, or if
    /// it cannot be, nothing is drawn and the result says why.
    pub fn image(&mut self, path: &str, rect: Rect, radius: f32) -> Lookup {
        // Don't decode what is scrolled out of view.
        if self.clip.intersect(rect).w <= 0.0 || self.clip.intersect(rect).h <= 0.0 {
            return Lookup::Loading;
        }
        let s = self.scale;
        let (left, top) = ((rect.x * s).round(), (rect.y * s).round());
        // The size alone decides the key: scrolling must not re-decode.
        let (width, height) = ((rect.w * s).round(), (rect.h * s).round());
        let (w, h) = (width.max(1.0) as u32, height.max(1.0) as u32);
        let key = ImageKey { path: path.to_owned(), w, h, whole: false };
        let lookup = self.images.lookup(&key);
        if let Lookup::Ready(uv) = lookup {
            // Exactly the thumbnail's pixels, so texels map one to one.
            self.push(
                Rect::new(left / s, top / s, width / s, height / s),
                [1.0; 4],
                [1.0; 4],
                [radius, 0.0, 0.0, KIND_IMAGE],
                uv,
            );
        }
        lookup
    }

    /// Draws the whole image file at `path` in the middle of `rect`, as
    /// large as fits it and never larger than the image is, on `backing`
    /// (which shows where the image is transparent), both with rounded
    /// corners. Returns where it was drawn; until it is decoded, or if it
    /// cannot be, nothing is drawn and the result says which.
    ///
    /// # Errors
    /// [`Lookup::Loading`] or [`Lookup::Failed`]: there is nothing to draw (yet).
    pub fn picture(&mut self, path: &str, rect: Rect, radius: f32, backing: Color) -> Result<Rect, Lookup> {
        /// The box is asked for in steps, so a window being resized does not decode at every size.
        const STEP: f32 = 32.0;
        let s = self.scale;
        let side = |length: f32, most: u32| ((length * s / STEP).floor() * STEP).clamp(STEP, most as f32) as u32;
        let key =
            ImageKey { path: path.to_owned(), w: side(rect.w, MAX_WHOLE.0), h: side(rect.h, MAX_WHOLE.1), whole: true };
        match self.images.lookup(&key) {
            Lookup::Ready(uv) => {
                // Exactly the picture's pixels, so texels map one to one.
                let left = ((rect.x + rect.w * 0.5) * s - uv[2] * 0.5).round();
                let top = ((rect.y + rect.h * 0.5) * s - uv[3] * 0.5).round();
                let drawn = Rect::new(left / s, top / s, uv[2] / s, uv[3] / s);
                self.rect(drawn, backing, radius);
                self.push(drawn, [1.0; 4], [1.0; 4], [radius, 0.0, 0.0, KIND_IMAGE], uv);
                Ok(drawn)
            }
            other => Err(other),
        }
    }

    /// Draws the image at `path` as a soft wash over `rect` (its corners
    /// rounded by `radius`): a thumbnail a few pixels across, stretched so
    /// the GPU's filtering blurs it, faded to `alpha`. One quad, so a
    /// backdrop costs nothing per frame.
    pub fn backdrop(&mut self, path: &str, rect: Rect, radius: f32, alpha: f32) -> Lookup {
        const SIDE: f32 = 24.0;
        let (w, h) = if rect.w >= rect.h {
            (SIDE, (SIDE * rect.h / rect.w).max(2.0))
        } else {
            ((SIDE * rect.w / rect.h).max(2.0), SIDE)
        };
        let key = ImageKey { path: path.to_owned(), w: w.round() as u32, h: h.round() as u32, whole: false };
        let lookup = self.images.lookup(&key);
        if let Lookup::Ready(uv) = lookup {
            // Inset half a texel, so filtering never reaches the atlas's
            // neighbouring images.
            let uv = [uv[0] + 0.5, uv[1] + 0.5, uv[2] - 1.0, uv[3] - 1.0];
            self.push(rect, [1.0, 1.0, 1.0, alpha], [1.0; 4], [radius, 0.0, 0.0, KIND_IMAGE], uv);
        }
        lookup
    }

    /// Restricts subsequent drawing to `rect` (intersected with the current
    /// clip) and returns the previous clip for [`Painter::set_clip`].
    pub fn push_clip(&mut self, rect: Rect) -> Rect {
        let clip = self.clip.intersect(rect);
        std::mem::replace(&mut self.clip, clip)
    }

    /// The current clip rectangle.
    #[must_use]
    pub fn clip(&self) -> Rect {
        self.clip
    }

    /// Restores a clip returned by [`Painter::push_clip`].
    pub fn set_clip(&mut self, clip: Rect) {
        self.clip = clip;
    }

    fn push(&mut self, rect: Rect, color: Color, color2: Color, params: [f32; 4], uv: [f32; 4]) {
        let c = self.clip;
        // Cheap CPU cull; the shader clips exactly. Pad for blur falloff.
        let pad = params[2] + 1.0;
        if rect.right() + pad < c.x
            || rect.x - pad > c.right()
            || rect.bottom() + pad < c.y
            || rect.y - pad > c.bottom()
        {
            return;
        }
        self.out.push(Instance {
            rect: [rect.x, rect.y, rect.w, rect.h],
            clip: [c.x, c.y, c.right(), c.bottom()],
            color,
            color2,
            uv,
            params,
        });
    }

    /// Filled rounded rectangle.
    pub fn rect(&mut self, rect: Rect, color: Color, radius: f32) {
        self.push(rect, color, color, [radius, 0.0, 0.0, KIND_SHAPE], [0.0; 4]);
    }

    /// Rounded rectangle with an inner border.
    pub fn bordered(&mut self, rect: Rect, fill: Color, radius: f32, border: f32, border_color: Color) {
        self.push(rect, fill, border_color, [radius, border, 0.0, KIND_SHAPE], [0.0; 4]);
    }

    /// Rounded rectangle shading from `top` to `bottom`, e.g. content
    /// fading out at a scroll edge.
    pub fn gradient(&mut self, rect: Rect, top: Color, bottom: Color, radius: f32) {
        self.push(rect, top, bottom, [radius, 0.0, 0.0, KIND_GRADIENT], [0.0; 4]);
    }

    /// Soft shadow or glow: `rect` blurred by `blur` logical pixels.
    pub fn shadow(&mut self, rect: Rect, color: Color, radius: f32, blur: f32) {
        self.push(rect, color, color, [radius, 0.0, blur.max(0.01), KIND_SHAPE], [0.0; 4]);
    }

    /// Lays out text with this frame's scale.
    #[must_use]
    pub fn layout(&self, text: &str, style: Style, max_width: Option<f32>) -> TextLayout {
        TextLayout::new(self.fonts, text, style, max_width, self.scale)
    }

    /// Draws a laid-out block with its top-left at `(x, y)`.
    pub fn text(&mut self, layout: &TextLayout, x: f32, y: f32, color: Color) {
        self.text_aligned(layout, x, y, Align::Left, 0.0, color);
    }

    /// Draws a laid-out block, aligning each line inside `width`.
    pub fn text_aligned(&mut self, layout: &TextLayout, x: f32, y: f32, align: Align, width: f32, color: Color) {
        self.glyphs(layout, (x, y), (align, width), &[color]);
    }

    /// Draws a rich layout: run decorations first (code backgrounds,
    /// underlines, strike-throughs), then glyphs. A run's `ink` indexes
    /// `inks`; out-of-range slots use `inks[0]`. `code_bg` fills
    /// [`BACKGROUND`](crate::text::BACKGROUND) runs.
    pub fn rich(&mut self, layout: &TextLayout, x: f32, y: f32, inks: &[Color], code_bg: Color) {
        use crate::text::{BACKGROUND, STRIKE, UNDERLINE};
        for d in layout.decorations() {
            let ink = inks.get(usize::from(d.run.ink)).or(inks.first()).copied().unwrap_or([1.0; 4]);
            if d.run.decoration & BACKGROUND != 0 {
                let pad = (d.h * 0.12).round();
                self.rect(Rect::new(x + d.x - 3.0, y + d.y + pad, d.w + 6.0, d.h - 2.0 * pad), code_bg, 4.0);
            }
            if d.run.decoration & UNDERLINE != 0 {
                self.rect(Rect::new(x + d.x, y + d.y + d.baseline + 2.0, d.w, 1.0), fade(ink, 0.5), 0.0);
            }
            if d.run.decoration & STRIKE != 0 {
                self.rect(Rect::new(x + d.x, y + d.y + d.baseline * 0.68, d.w, 1.0), ink, 0.0);
            }
        }
        self.glyphs(layout, (x, y), (Align::Left, 0.0), inks);
    }

    fn glyphs(&mut self, layout: &TextLayout, origin: (f32, f32), align: (Align, f32), inks: &[Color]) {
        let clip = self.clip;
        let out = &mut *self.out;
        let fallback = inks.first().copied().unwrap_or([1.0; 4]);
        let ok =
            layout.place(self.fonts, self.atlas, origin, align, [clip.x, clip.y, clip.right(), clip.bottom()], |g| {
                let ink = inks.get(usize::from(g.ink)).copied().unwrap_or(fallback);
                // Emoji layers keep their own colours but follow the text's fade.
                let color = g.color.map_or(ink, |c| [c[0], c[1], c[2], c[3] * ink[3]]);
                out.push(Instance {
                    rect: g.rect,
                    clip: [clip.x, clip.y, clip.right(), clip.bottom()],
                    color,
                    color2: color,
                    uv: g.uv,
                    params: [0.0, 0.0, 0.0, KIND_GLYPH],
                });
            });
        self.atlas_full |= !ok;
    }

    /// Draws a platform's mark, `size` logical pixels square, centred on
    /// `center`, in `color`: rasterised from its outline at exactly this
    /// size and kept in the glyph atlas, like a letter.
    pub fn logo(&mut self, logo: Logo, center: (f32, f32), size: f32, color: Color) {
        let s = self.scale;
        let px = (size * s).round().max(1.0);
        let Some(entry) = self.atlas.mark((logo as u16, px.to_bits()), || logo.rasterize(px)) else {
            self.atlas_full = true;
            return;
        };
        if entry.w == 0 {
            return;
        }
        let (w, h) = (f32::from(entry.w), f32::from(entry.h));
        // The bitmap sits where its ink is within the square, measured from the square's bottom-left.
        let left = (center.0 * s - px * 0.5).round() + f32::from(entry.xmin);
        let top = (center.1 * s - px * 0.5).round() + px - f32::from(entry.ymin) - h;
        let clip = self.clip;
        self.out.push(Instance {
            rect: [left / s, top / s, w / s, h / s],
            clip: [clip.x, clip.y, clip.right(), clip.bottom()],
            color,
            color2: color,
            uv: [f32::from(entry.x), f32::from(entry.y), w, h],
            params: [0.0, 0.0, 0.0, KIND_GLYPH],
        });
    }

    /// Lays out and draws a single line; returns its width.
    pub fn label(&mut self, text: &str, style: Style, x: f32, y: f32, color: Color) -> f32 {
        let layout = self.layout(text, style, None);
        self.text(&layout, x, y, color);
        layout.width()
    }

    /// Draws a single line centred in `rect`.
    pub fn label_centered(&mut self, text: &str, style: Style, rect: Rect, color: Color) {
        let layout = self.layout(text, style, None);
        let x = rect.x + (rect.w - layout.width()) * 0.5;
        let y = rect.y + (rect.h - layout.height()) * 0.5;
        self.text(&layout, x, y, color);
    }
}
