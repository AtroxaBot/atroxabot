//! The marks of the platforms an agent can think through (Claude, `OpenAI`,
//! `OpenRouter`, …), drawn from their outlines at whatever size is asked
//! for, in one colour, through the glyph atlas: a logo costs what a letter
//! costs. The outlines are SVG path data on a 24-unit square (from Simple
//! Icons, CC0); the marks themselves are their owners' trademarks, shown
//! only to say which platform a provider is. The app's own mark, three
//! bodies in a line, is drawn the same way.

use ttf_parser::OutlineBuilder;

use crate::raster::{self, Bitmap};

/// The side of the square every outline is drawn on.
const BOX: f32 = 24.0;

/// A platform's mark, or the app's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Logo {
    /// The app's own: a planet and two moons in a line.
    App,
    /// Claude's spark.
    Claude,
    /// Anthropic's "A".
    Anthropic,
    /// `OpenAI`'s knot (also Codex).
    OpenAi,
    /// `OpenRouter`'s arrows.
    OpenRouter,
    /// Meta's loop (also Muse).
    Meta,
    /// Gemini's star.
    Gemini,
    /// Mistral's blocks.
    Mistral,
    /// Ollama's llama.
    Ollama,
}

impl Logo {
    /// Every mark, for the tests.
    #[cfg(test)]
    const ALL: [Self; 9] = [
        Self::App,
        Self::Claude,
        Self::Anthropic,
        Self::OpenAi,
        Self::OpenRouter,
        Self::Meta,
        Self::Gemini,
        Self::Mistral,
        Self::Ollama,
    ];

    /// The mark of the platform called (or reached at an address holding)
    /// `name`, if the app has one.
    #[must_use]
    pub fn named(name: &str) -> Option<Self> {
        let name = name.to_lowercase();
        let marks = [
            ("claude", Self::Claude),
            ("anthropic", Self::Anthropic),
            ("openrouter", Self::OpenRouter),
            ("openai", Self::OpenAi),
            ("codex", Self::OpenAi),
            ("meta", Self::Meta),
            ("muse", Self::Meta),
            ("google", Self::Gemini),
            ("gemini", Self::Gemini),
            ("mistral", Self::Mistral),
            ("ollama", Self::Ollama),
        ];
        marks.iter().find(|(platform, _)| name.contains(platform)).map(|(_, logo)| *logo)
    }

    /// Its outline, as SVG path data.
    fn path(self) -> &'static str {
        match self {
            // Three discs along a rising diagonal, each less than half the last.
            Self::App => {
                "M3.375 15.375a5.25 5.25 0 1 0 10.5 0a5.25 5.25 0 1 0-10.5 0z\
                 M12.2815 8.906a2.8125 2.8125 0 1 0 5.625 0a2.8125 2.8125 0 1 0-5.625 0z\
                 M17.3435 5.344a1.3125 1.3125 0 1 0 2.625 0a1.3125 1.3125 0 1 0-2.625 0z"
            }
            Self::Claude => include_str!("../assets/logos/claude.path"),
            Self::Anthropic => include_str!("../assets/logos/anthropic.path"),
            Self::OpenAi => include_str!("../assets/logos/openai.path"),
            Self::OpenRouter => include_str!("../assets/logos/openrouter.path"),
            Self::Meta => include_str!("../assets/logos/meta.path"),
            Self::Gemini => include_str!("../assets/logos/googlegemini.path"),
            Self::Mistral => include_str!("../assets/logos/mistralai.path"),
            Self::Ollama => include_str!("../assets/logos/ollama.path"),
        }
    }

    /// Its coverage on a square `px` pixels a side: a bitmap placed
    /// relative to the square's bottom-left corner, y up, as a glyph's is
    /// to its pen. `None` if it has no ink at that size.
    #[must_use]
    pub fn rasterize(self, px: f32) -> Option<Bitmap> {
        raster::rasterize_outline(px / BOX, |sink| outline(self.path(), sink))
    }
}

/// Reads numbers and command letters out of path data.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    /// Skips the spaces and commas between things.
    fn skip(&mut self) {
        while self.bytes.get(self.at).is_some_and(|b| b.is_ascii_whitespace() || *b == b',') {
            self.at += 1;
        }
    }

    /// The command letter that comes next, if one does.
    fn command(&mut self) -> Option<u8> {
        self.skip();
        let letter = self.bytes.get(self.at).copied().filter(u8::is_ascii_alphabetic)?;
        self.at += 1;
        Some(letter)
    }

    /// The number that comes next: path data runs them together
    /// (`1.5.25-3`), so one ends where the next can only begin.
    fn number(&mut self) -> Option<f32> {
        self.skip();
        let start = self.at;
        let mut seen_dot = false;
        while let Some(byte) = self.bytes.get(self.at).copied() {
            let first = self.at == start;
            let after_exponent = !first && matches!(self.bytes[self.at - 1], b'e' | b'E');
            let part = match byte {
                b'0'..=b'9' => true,
                b'-' | b'+' => first || after_exponent,
                b'.' if !seen_dot => {
                    seen_dot = true;
                    true
                }
                b'e' | b'E' => !first && self.bytes.get(self.at + 1).is_some_and(|next| !next.is_ascii_alphabetic()),
                _ => false,
            };
            if !part {
                break;
            }
            self.at += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.at]).ok()?.parse().ok().filter(|number: &f32| number.is_finite())
    }

    /// One of an arc's two flags: a single `0` or `1`, often not set apart from what follows.
    fn flag(&mut self) -> Option<bool> {
        self.skip();
        let flag = match self.bytes.get(self.at)? {
            b'0' => false,
            b'1' => true,
            _ => return None,
        };
        self.at += 1;
        Some(flag)
    }
}

/// Draws SVG path data into `sink`, turned y up. What cannot be read ends
/// the outline there.
fn outline(data: &str, sink: &mut dyn OutlineBuilder) {
    let mut reader = Reader { bytes: data.as_bytes(), at: 0 };
    // Where the pen is, where the subpath began, and the last control point (for `S` and `T`).
    let (mut pen, mut start, mut control) = ((0.0_f32, 0.0_f32), (0.0_f32, 0.0_f32), None::<(f32, f32)>);
    let mut command = 0_u8;
    let up = |(x, y): (f32, f32)| (x, BOX - y);
    loop {
        match reader.command() {
            Some(letter) => command = letter,
            // More numbers after a command repeat it; a move repeats as lines.
            None if reader.at < reader.bytes.len() && command != 0 => {
                command = match command {
                    b'M' => b'L',
                    b'm' => b'l',
                    other => other,
                };
            }
            None => return,
        }
        let relative = command.is_ascii_lowercase();
        let from = pen;
        let point = |reader: &mut Reader<'_>| {
            let (x, y) = (reader.number()?, reader.number()?);
            Some(if relative { (from.0 + x, from.1 + y) } else { (x, y) })
        };
        let mut next_control = None;
        match command.to_ascii_uppercase() {
            b'M' => {
                let Some(to) = point(&mut reader) else { return };
                sink.move_to(up(to).0, up(to).1);
                (pen, start) = (to, to);
            }
            b'L' => {
                let Some(to) = point(&mut reader) else { return };
                sink.line_to(up(to).0, up(to).1);
                pen = to;
            }
            b'H' | b'V' => {
                let Some(value) = reader.number() else { return };
                let to = match (command, relative) {
                    (b'H', _) => (value, pen.1),
                    (b'h', _) => (pen.0 + value, pen.1),
                    (b'V', _) => (pen.0, value),
                    _ => (pen.0, pen.1 + value),
                };
                sink.line_to(up(to).0, up(to).1);
                pen = to;
            }
            kind @ (b'C' | b'S') => {
                // `S` mirrors the last curve's second control point.
                let first = if kind == b'C' {
                    point(&mut reader)
                } else {
                    Some(control.map_or(pen, |last| (2.0 * pen.0 - last.0, 2.0 * pen.1 - last.1)))
                };
                let (Some(c1), Some(c2), Some(to)) = (first, point(&mut reader), point(&mut reader)) else { return };
                sink.curve_to(up(c1).0, up(c1).1, up(c2).0, up(c2).1, up(to).0, up(to).1);
                (pen, next_control) = (to, Some(c2));
            }
            kind @ (b'Q' | b'T') => {
                let first = if kind == b'Q' {
                    point(&mut reader)
                } else {
                    Some(control.map_or(pen, |last| (2.0 * pen.0 - last.0, 2.0 * pen.1 - last.1)))
                };
                let (Some(c), Some(to)) = (first, point(&mut reader)) else { return };
                sink.quad_to(up(c).0, up(c).1, up(to).0, up(to).1);
                (pen, next_control) = (to, Some(c));
            }
            b'A' => {
                let (Some(rx), Some(ry), Some(turn)) = (reader.number(), reader.number(), reader.number()) else {
                    return;
                };
                let (Some(large), Some(sweep), Some(to)) = (reader.flag(), reader.flag(), point(&mut reader)) else {
                    return;
                };
                arc(sink, pen, to, (rx, ry, turn), (large, sweep));
                pen = to;
            }
            b'Z' => {
                sink.close();
                pen = start;
            }
            _ => return,
        }
        control = next_control;
    }
}

/// An elliptical arc from `from` to `to` (SVG's `A`), as cubic curves into
/// `sink`, turned y up: the conversion of the SVG specification, F.6.5.
fn arc(
    sink: &mut dyn OutlineBuilder,
    from: (f32, f32),
    to: (f32, f32),
    (rx, ry, turn): (f32, f32, f32),
    flags: (bool, bool),
) {
    let (mut rx, mut ry) = (rx.abs(), ry.abs());
    let line = |sink: &mut dyn OutlineBuilder| sink.line_to(to.0, BOX - to.1);
    if rx < 1e-6 || ry < 1e-6 || from == to {
        return line(sink);
    }
    let (sin, cos) = turn.to_radians().sin_cos();
    // The midpoint between the ends, in the ellipse's own axes.
    let (dx, dy) = ((from.0 - to.0) * 0.5, (from.1 - to.1) * 0.5);
    let (x1, y1) = (cos * dx + sin * dy, -sin * dx + cos * dy);
    // Radii too small to reach are grown until they do.
    let reach = (x1 * x1) / (rx * rx) + (y1 * y1) / (ry * ry);
    if reach > 1.0 {
        (rx, ry) = (rx * reach.sqrt(), ry * reach.sqrt());
    }
    let (rx2, ry2) = (rx * rx, ry * ry);
    let under = rx2 * y1 * y1 + ry2 * x1 * x1;
    let factor = ((rx2 * ry2 - under).max(0.0) / under).sqrt() * if flags.0 == flags.1 { -1.0 } else { 1.0 };
    let (cx1, cy1) = (factor * rx * y1 / ry, -factor * ry * x1 / rx);
    let center =
        (cos * cx1 - sin * cy1 + f32::midpoint(from.0, to.0), sin * cx1 + cos * cy1 + f32::midpoint(from.1, to.1));
    let angle = |x: f32, y: f32| y.atan2(x);
    let first = angle((x1 - cx1) / rx, (y1 - cy1) / ry);
    let mut sweep = angle((-x1 - cx1) / rx, (-y1 - cy1) / ry) - first;
    if flags.1 && sweep < 0.0 {
        sweep += std::f32::consts::TAU;
    } else if !flags.1 && sweep > 0.0 {
        sweep -= std::f32::consts::TAU;
    }
    if !sweep.is_finite() {
        return line(sink);
    }
    // A quarter turn at most per curve keeps each within a hair of the arc.
    let pieces = (sweep.abs() / std::f32::consts::FRAC_PI_2).ceil().clamp(1.0, 4.0);
    let step = sweep / pieces;
    let handle = 4.0 / 3.0 * (step * 0.25).tan();
    let on = |theta: f32| {
        let (s, c) = theta.sin_cos();
        let at = (center.0 + rx * c * cos - ry * s * sin, center.1 + rx * c * sin + ry * s * cos);
        let along = (-rx * s * cos - ry * c * sin, -rx * s * sin + ry * c * cos);
        (at, along)
    };
    for piece in 0..pieces as u32 {
        let (a, b) = (first + step * piece as f32, first + step * (piece + 1) as f32);
        let ((p0, d0), (p1, d1)) = (on(a), on(b));
        let (c1, c2) = ((p0.0 + handle * d0.0, p0.1 + handle * d0.1), (p1.0 - handle * d1.0, p1.1 - handle * d1.1));
        sink.curve_to(c1.0, BOX - c1.1, c2.0, BOX - c2.1, p1.0, BOX - p1.1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The share of a `px` square that `bitmap` inks.
    fn ink(bitmap: &Bitmap, px: f32) -> f32 {
        bitmap.data.iter().map(|coverage| f32::from(*coverage) / 255.0).sum::<f32>() / (px * px)
    }

    #[test]
    fn every_mark_draws_inside_its_square() {
        for logo in Logo::ALL {
            let bitmap = logo.rasterize(64.0).unwrap_or_else(|| panic!("{logo:?} has no ink"));
            let share = ink(&bitmap, 64.0);
            assert!((0.08..0.9).contains(&share), "{logo:?} inks {share} of its square");
            let (right, top) = (bitmap.xmin + bitmap.w as i32, bitmap.ymin + bitmap.h as i32);
            assert!(bitmap.xmin >= -1 && bitmap.ymin >= -1 && right <= 65 && top <= 65, "{logo:?} leaves its square");
        }
        assert_eq!(
            (Logo::named("api.OpenAI.com"), Logo::named("openrouter")),
            (Some(Logo::OpenAi), Some(Logo::OpenRouter))
        );
        assert_eq!((Logo::named("Claude"), Logo::named("localhost")), (Some(Logo::Claude), None));
    }

    #[test]
    fn path_data_is_read_as_svg_writes_it() {
        let draw = |data: &'static str| raster::rasterize_outline(1.0, |sink| outline(data, sink));
        // A square, written three ways: numbers run together, relative moves, repeated commands.
        for data in ["M4 4H20V20H4Z", "m4,4 16 0 0 16-16 0z", "M4 4l16 0 0 16L4 20z"] {
            let square = draw(data).expect("a square");
            assert!((ink(&square, 24.0) - 256.0 / 576.0).abs() < 0.01, "{data}");
        }
        // A circle of two arcs, flags run into what follows.
        let circle = draw("M4 12a8 8 0 1016 0a8 8 0 10-16 0z").expect("a circle");
        let area = std::f32::consts::PI * 64.0 / 576.0;
        assert!((ink(&circle, 24.0) - area).abs() < 0.01, "{}", ink(&circle, 24.0));
        // Curves, and what is not path data at all.
        assert!(draw("M2 12Q12 2 22 12T2 12zM8 8c2-2 6-2 8 0s2 6 0 8z").is_some());
        for junk in ["", "hello", "M", "M1", "M1 1L", "M0 0A1 1 0 2 0 5 5", "Z", "M1e999 0L5 5 9 0z"] {
            let _ = draw(junk);
        }
    }
}
