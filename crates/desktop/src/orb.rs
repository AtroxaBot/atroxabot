//! An agent's face: a soft orb in its colour with two eyes. Small in the
//! sidebar and the header, large and alive on a call, where it breathes with
//! the voices and looks around while the agent thinks.
//!
//! Everything is a handful of quads of the one pipeline (a gradient disc,
//! blurred discs for the glow, rounded rectangles for the eyes), so a face
//! costs nothing to draw and animates only while its agent is busy.

use crate::image::Lookup;
use crate::paint::{Color, Painter, Rect, fade, hex, mix};

/// What an agent is doing, which its face shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mood {
    /// Nothing: a still face.
    Idle,
    /// Working on a reply: it glows and looks around.
    Thinking,
    /// On a call, waiting for the user to speak.
    Listening,
    /// The user is speaking; `0..=1` is how loud.
    Hearing(f32),
    /// The agent is speaking; `0..=1` is how loud.
    Speaking(f32),
}

impl Mood {
    /// Whether the face moves, so the frame must be drawn again.
    #[must_use]
    pub fn animated(self) -> bool {
        self != Self::Idle
    }
}

/// A disc of diameter `size` centred on `center`.
fn disc((cx, cy): (f32, f32), size: f32) -> Rect {
    Rect::new(cx - size * 0.5, cy - size * 0.5, size, size)
}

/// Draws the face of an agent coloured `color`, `size` across, centred on
/// `center`, at `time` seconds (for its motion).
pub fn face(p: &mut Painter, center: (f32, f32), size: f32, color: Color, mood: Mood, time: f32) {
    let light = p.theme.is_light();
    let (level, glow) = match mood {
        Mood::Idle => (0.0, 0.0),
        Mood::Thinking => (0.0, 0.5 + 0.5 * (time * 2.4).sin()),
        Mood::Listening => (0.0, 0.25 + 0.15 * (time * 1.3).sin()),
        Mood::Hearing(level) | Mood::Speaking(level) => (level.clamp(0.0, 1.0), 0.4 + 0.6 * level.clamp(0.0, 1.0)),
    };
    // Speech swells the orb; listening leans in a little.
    let swell = match mood {
        Mood::Speaking(_) => 1.0 + 0.10 * level,
        Mood::Hearing(_) => 1.0 - 0.04 * level,
        _ => 1.0,
    };
    let body = size * swell;
    if glow > 0.0 {
        let strength = if light { 0.30 } else { 0.42 };
        p.shadow(disc(center, body * 1.02), fade(color, strength * glow), body * 0.51, size * (0.22 + 0.30 * glow));
    }
    // Lit from above: lighter at the top, deeper at the foot.
    let top = mix(color, hex(0xFFFFFF), 0.30);
    let foot = mix(color, hex(0x000000), 0.22);
    p.gradient(disc(center, body), top, foot, body * 0.5);
    // A soft sheen on the upper left.
    let sheen = Rect::new(center.0 - body * 0.30, center.1 - body * 0.36, body * 0.34, body * 0.20);
    p.shadow(sheen, fade(hex(0xFFFFFF), 0.28), body * 0.1, body * 0.10);

    // Eyes: they wander while thinking, widen when hearing, and blink.
    let look = match mood {
        Mood::Thinking => ((time * 1.7).sin() * 0.07, (time * 1.1).cos() * 0.035 - 0.03),
        Mood::Hearing(_) => (0.0, -0.02),
        _ => (0.0, 0.0),
    };
    let blinking = mood.animated() && (time * 0.37).fract() < 0.035;
    let open = match mood {
        _ if blinking => 0.12,
        Mood::Hearing(_) => 1.0 + 0.25 * level,
        Mood::Speaking(_) => 0.85 + 0.15 * level,
        _ => 1.0,
    };
    let (eye_w, eye_h) = ((body * 0.115).max(1.5), (body * 0.215 * open).max(1.5));
    let ink = fade(hex(0x101014), 0.88);
    for side in [-1.0, 1.0] {
        let x = center.0 + side * body * 0.145 + look.0 * body - eye_w * 0.5;
        let y = center.1 - body * 0.02 + look.1 * body - eye_h * 0.5;
        p.rect(Rect::new(x, y, eye_w, eye_h), ink, eye_w * 0.5);
    }
}

/// Draws an agent: its picture at `portrait` when it has one that can be
/// shown, glowing in its colour while it is busy, and its drawn face otherwise.
pub fn agent(p: &mut Painter, portrait: &str, center: (f32, f32), size: f32, color: Color, mood: Mood, time: f32) {
    if portrait.is_empty() {
        face(p, center, size, color, mood, time);
        return;
    }
    if mood.animated() {
        let pulse = 0.5 + 0.5 * (time * 2.4).sin();
        p.shadow(disc(center, size * 1.04), fade(color, 0.25 + 0.3 * pulse), size * 0.52, size * 0.3);
    }
    // Until the picture is decoded (or if it cannot be), the face stands in.
    if !matches!(p.image(portrait, disc(center, size), size * 0.5), Lookup::Ready(_)) {
        face(p, center, size, color, mood, time);
    }
}

/// Draws the large face of a call, `size` across: the agent inside rings
/// that ripple with whoever is speaking.
pub fn reactor(p: &mut Painter, portrait: &str, center: (f32, f32), size: f32, color: Color, mood: Mood, time: f32) {
    let level = match mood {
        Mood::Hearing(level) | Mood::Speaking(level) => level.clamp(0.0, 1.0),
        _ => 0.0,
    };
    // Three rings drift outwards; sound pushes them further and brighter.
    for ring in 0..3u8 {
        let phase = (time * 0.35 + f32::from(ring) / 3.0).fract();
        let reach = size * (1.05 + (0.55 + 0.5 * level) * phase);
        let alpha = (1.0 - phase) * (0.10 + 0.35 * level) * if mood == Mood::Idle { 0.0 } else { 1.0 };
        p.bordered(disc(center, reach), fade(color, 0.0), reach * 0.5, 1.5, fade(color, alpha));
    }
    // While thinking, three sparks orbit the face.
    if mood == Mood::Thinking {
        for spark in 0..3u8 {
            let angle = time * 1.8 + f32::from(spark) * std::f32::consts::TAU / 3.0;
            let at = (center.0 + angle.cos() * size * 0.72, center.1 + angle.sin() * size * 0.72);
            p.shadow(disc(at, size * 0.07), fade(color, 0.9), size * 0.035, size * 0.04);
        }
    }
    agent(p, portrait, center, size, color, mood, time);
}
