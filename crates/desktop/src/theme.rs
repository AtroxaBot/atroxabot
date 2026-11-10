//! Visual design tokens. Every colour and size in the UI comes from here.
//!
//! Colours live in a [`Palette`] so the scheme can change at runtime; the
//! painter carries the active one (`p.theme`). Sizes and text styles are the
//! same in every scheme. The story reads like a book: prose in a serif, and
//! every character in a colour of their own
//! ([`character_color`]), so a busy scene reads at a glance.

use crate::paint::{Color, hex, hexa, mix};
use crate::text::{FontId, Style};

/// Every colour the UI uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// Chat area background.
    pub bg: Color,
    /// Sidebar and other panels.
    pub panel: Color,
    /// Raised surfaces: composer, cards, menus.
    pub surface: Color,
    /// Hovered element.
    pub hover: Color,
    /// Selected or pressed element.
    pub active: Color,
    /// Hairline dividers.
    pub border: Color,
    /// Borders of controls and floating surfaces.
    pub border_strong: Color,
    /// Border of the focused input.
    pub border_focus: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text.
    pub text_muted: Color,
    /// Hints, placeholders and captions.
    pub text_faint: Color,
    /// The single accent colour.
    pub accent: Color,
    /// Text and icons drawn on the accent colour.
    pub on_accent: Color,
    /// Text selection highlight.
    pub selection: Color,
    /// Errors and destructive actions; lines a diff removes.
    pub danger: Color,
    /// Lines a diff adds.
    pub positive: Color,
    /// Drop shadows under floating surfaces.
    pub shadow: Color,
    /// Background of code blocks.
    pub code_bg: Color,
    /// Links in replies.
    pub link: Color,
    /// Laid over a picture so text on it stays legible.
    pub scrim: Color,
    /// Text drawn on [`Palette::scrim`].
    pub on_scrim: Color,
    /// Syntax colours: keyword, string, number, comment, function, type.
    pub syntax: [Color; 6],
}

impl Palette {
    /// Blends every colour towards `other` by `t` (for theme transitions).
    #[must_use]
    pub fn mix(&self, other: &Self, t: f32) -> Self {
        let m = |a, b| mix(a, b, t);
        Self {
            bg: m(self.bg, other.bg),
            panel: m(self.panel, other.panel),
            surface: m(self.surface, other.surface),
            hover: m(self.hover, other.hover),
            active: m(self.active, other.active),
            border: m(self.border, other.border),
            border_strong: m(self.border_strong, other.border_strong),
            border_focus: m(self.border_focus, other.border_focus),
            text: m(self.text, other.text),
            text_muted: m(self.text_muted, other.text_muted),
            text_faint: m(self.text_faint, other.text_faint),
            accent: m(self.accent, other.accent),
            on_accent: m(self.on_accent, other.on_accent),
            selection: m(self.selection, other.selection),
            danger: m(self.danger, other.danger),
            positive: m(self.positive, other.positive),
            shadow: m(self.shadow, other.shadow),
            code_bg: m(self.code_bg, other.code_bg),
            link: m(self.link, other.link),
            scrim: m(self.scrim, other.scrim),
            on_scrim: m(self.on_scrim, other.on_scrim),
            syntax: std::array::from_fn(|i| m(self.syntax[i], other.syntax[i])),
        }
    }

    /// Whether text is dark on a light background.
    #[must_use]
    pub fn is_light(&self) -> bool {
        self.bg[0] + self.bg[1] + self.bg[2] > 1.5
    }
}

/// Near-black, lit only by white: the default.
pub const BLACK: Palette = Palette {
    bg: hex(0x121212),
    panel: hex(0x080808),
    surface: hex(0x1A1A1A),
    hover: hex(0x1F1F1F),
    active: hex(0x282828),
    border: hex(0x222222),
    border_strong: hex(0x303030),
    border_focus: hex(0x4C4C4C),
    text: hex(0xEDEDED),
    text_muted: hex(0xA3A3A3),
    text_faint: hex(0x6B6B6B),
    accent: hex(0xEDEDED),
    on_accent: hex(0x0A0A0A),
    selection: hexa(0xFFFFFF, 0.16),
    danger: hex(0xF26D6D),
    positive: hex(0x6CCB8B),
    shadow: hexa(0x000000, 0.6),
    code_bg: hex(0x171717),
    link: hex(0x7DB3FF),
    scrim: hexa(0x000000, 0.6),
    on_scrim: hex(0xF5F5F5),
    // GitHub Dark.
    syntax: [hex(0xFF7B72), hex(0xA5D6FF), hex(0x79C0FF), hex(0x8B949E), hex(0xD2A8FF), hex(0xFFA657)],
};

/// Clean white with ink-black controls.
pub const WHITE: Palette = Palette {
    bg: hex(0xFFFFFF),
    panel: hex(0xF4F4F5),
    surface: hex(0xFFFFFF),
    hover: hex(0xECECEE),
    active: hex(0xE4E4E7),
    border: hex(0xE6E6E9),
    border_strong: hex(0xD6D6DB),
    border_focus: hex(0xA1A1AA),
    text: hex(0x18181B),
    text_muted: hex(0x55555E),
    text_faint: hex(0x9B9BA4),
    accent: hex(0x18181B),
    on_accent: hex(0xFAFAFA),
    selection: hexa(0x2563EB, 0.16),
    danger: hex(0xDC2626),
    positive: hex(0x15803D),
    shadow: hexa(0x0F0F14, 0.09),
    code_bg: hex(0xF6F6F7),
    link: hex(0x2563EB),
    scrim: hexa(0x0C0C0F, 0.55),
    on_scrim: hex(0xFFFFFF),
    // GitHub Light.
    syntax: [hex(0xCF222E), hex(0x0A3069), hex(0x0550AE), hex(0x6E7781), hex(0x8250DF), hex(0x953800)],
};

/// Warm cream and sepia with a copper accent: light, with little blue.
pub const AMBER: Palette = Palette {
    bg: hex(0xF8F3EA),
    panel: hex(0xEFE8DC),
    surface: hex(0xFDF9F2),
    hover: hex(0xEAE1D2),
    active: hex(0xE2D7C4),
    border: hex(0xE5DBCA),
    border_strong: hex(0xD5C7B0),
    border_focus: hex(0xAE9A79),
    text: hex(0x2E2419),
    text_muted: hex(0x6E5F4C),
    text_faint: hex(0xA3927A),
    accent: hex(0xBF6129),
    on_accent: hex(0xFFF8EF),
    selection: hexa(0xBF6129, 0.2),
    danger: hex(0xB93A2C),
    positive: hex(0x4D7C2A),
    shadow: hexa(0x3C280F, 0.1),
    code_bg: hex(0xF1EADD),
    link: hex(0xA3501C),
    scrim: hexa(0x1E140A, 0.55),
    on_scrim: hex(0xFFF9F0),
    // Gruvbox Light.
    syntax: [hex(0x9D0006), hex(0x79740E), hex(0x8F3F71), hex(0x928374), hex(0x076678), hex(0xB57614)],
};

/// The colour schemes offered in settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scheme {
    /// [`BLACK`].
    #[default]
    Black,
    /// [`WHITE`].
    White,
    /// [`AMBER`].
    Amber,
}

impl Scheme {
    /// Every scheme, in settings order.
    pub const ALL: [Self; 3] = [Self::Black, Self::White, Self::Amber];

    /// Its colours.
    #[must_use]
    pub fn palette(self) -> &'static Palette {
        match self {
            Self::Black => &BLACK,
            Self::White => &WHITE,
            Self::Amber => &AMBER,
        }
    }

    /// Name shown in settings.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Black => "Black",
            Self::White => "White",
            Self::Amber => "Amber",
        }
    }

    /// Value stored in the config file.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::White => "white",
            Self::Amber => "amber",
        }
    }

    /// Parses a config value; unknown values fall back to the default.
    /// Schemes since retired map to the nearest one left.
    #[must_use]
    pub fn from_key(key: Option<&str>) -> Self {
        match key {
            Some("light") => Self::White,
            Some("parchment") => Self::Amber,
            _ => Self::ALL.into_iter().find(|s| Some(s.key()) == key).unwrap_or_default(),
        }
    }

    /// Whether the OS chrome (title bar) should be light.
    #[must_use]
    pub fn is_light(self) -> bool {
        self.palette().is_light()
    }
}

/// The colour of the agent called `name`, the same wherever its face
/// appears and on every scheme: the one the user `chose` (`#rrggbb`), or,
/// with none, one picked from the name, so the same name always gets the
/// same colour.
#[must_use]
pub fn agent_color(name: &str, chose: &str) -> Color {
    if let Some([r, g, b]) = parse_hex(chose) {
        return [r, g, b, 1.0];
    }
    // FNV-1a over the name as names are compared: case and marks ignored.
    let mut hash: u32 = 0x811C_9DC5;
    for c in name.chars().flat_map(char::to_lowercase).filter(|c| c.is_alphanumeric()) {
        hash = (hash ^ c as u32).wrapping_mul(0x0100_0193);
    }
    // Bright and soft: a face reads on dark and light canvases alike.
    oklch(0.78, 0.115, (hash % 360) as f32)
}

/// The colour a platform's mark is known in, for a provider called (or
/// filed under) `name`: Claude's orange, Meta's blue, and so on. `None` for
/// a mark that is black or white by its owner's design (`OpenAI`,
/// Anthropic, `OpenRouter`) or that the app knows no colour of: those are
/// drawn in the theme's own ink.
#[must_use]
pub fn provider_color(name: &str) -> Option<Color> {
    let name = name.to_lowercase();
    let colors = [
        ("claude", 0xD9_7757),
        ("meta", 0x00_82FB),
        ("muse", 0x00_82FB),
        ("google", 0x42_85F4),
        ("gemini", 0x42_85F4),
        ("mistral", 0xFA_520F),
        ("groq", 0xF5_5036),
        ("deepseek", 0x4D_6BFE),
    ];
    colors.iter().find(|(platform, _)| name.contains(platform)).map(|(_, rgb)| hex(*rgb))
}

/// An sRGB colour (0–1 each) from `#rrggbb`, `rrggbb` or `#rgb`.
#[must_use]
pub fn parse_hex(text: &str) -> Option<[f32; 3]> {
    let digits = text.trim().trim_start_matches('#');
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |at: usize, width: usize| {
        let value = u8::from_str_radix(&digits[at * width..(at + 1) * width], 16).ok()?;
        Some(f32::from(if width == 1 { value * 17 } else { value }) / 255.0)
    };
    let width = match digits.len() {
        3 => 1,
        6 => 2,
        _ => return None,
    };
    Some([channel(0, width)?, channel(1, width)?, channel(2, width)?])
}

/// `rgb` (sRGB, 0–1 each) as `#rrggbb`.
#[must_use]
pub fn to_hex(rgb: [f32; 3]) -> String {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(rgb[0]), byte(rgb[1]), byte(rgb[2]))
}

/// An opaque sRGB colour from OKLCH (perceptual lightness 0–1, chroma,
/// hue in degrees), its chroma reduced until it fits sRGB.
fn oklch(lightness: f32, chroma: f32, hue: f32) -> Color {
    let (sin, cos) = hue.to_radians().sin_cos();
    let mut chroma = chroma;
    loop {
        // OKLab's axes, then the cone responses (long, medium, short).
        let (green_red, blue_yellow) = (chroma * cos, chroma * sin);
        let long = (lightness + 0.396_337_8 * green_red + 0.215_803_76 * blue_yellow).powi(3);
        let medium = (lightness - 0.105_561_35 * green_red - 0.063_854_17 * blue_yellow).powi(3);
        let short = (lightness - 0.089_484_18 * green_red - 1.291_485_5 * blue_yellow).powi(3);
        let linear = [
            4.076_741_7 * long - 3.307_711_6 * medium + 0.230_969_94 * short,
            -1.268_438 * long + 2.609_757_4 * medium - 0.341_319_38 * short,
            -0.004_196_086 * long - 0.703_418_6 * medium + 1.707_614_7 * short,
        ];
        if chroma < 0.005 || linear.iter().all(|c| (0.0..=1.0).contains(c)) {
            let encode = |c: f32| {
                let c = c.clamp(0.0, 1.0);
                if c <= 0.003_130_8 { 12.92 * c } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
            };
            return [encode(linear[0]), encode(linear[1]), encode(linear[2]), 1.0];
        }
        chroma *= 0.85;
    }
}

/// Corner radius of small elements: list rows, buttons.
pub const RADIUS_SM: f32 = 7.0;
/// Corner radius of containers: menus, cards, dialogs.
pub const RADIUS: f32 = 10.0;
/// Corner radius of large surfaces: the composer and bubbles.
pub const RADIUS_LG: f32 = 16.0;
/// Corner radius of the canvas the main area sits on.
pub const CANVAS_RADIUS: f32 = 12.0;
/// Space between the window's edge and the canvas.
pub const GAP: f32 = 8.0;
/// Height of the title bar strip along the window's top.
pub const TITLE_HEIGHT: f32 = 36.0;
/// Sidebar width.
pub const SIDEBAR_WIDTH: f32 = 260.0;
/// Height of the header bar above the main area.
pub const HEADER_HEIGHT: f32 = 52.0;
/// Maximum width of the message column.
pub const COLUMN_WIDTH: f32 = 760.0;

/// Message body text.
pub const BODY: Style = Style::regular(14.5);
/// Secondary UI text.
pub const SMALL: Style = Style::regular(13.0);
/// Captions and hints.
pub const TINY: Style = Style::regular(11.5);
/// Buttons and list items.
pub const LABEL: Style = Style::semibold(13.0);
/// Small headings and captions that need weight.
pub const CAPTION: Style = Style::semibold(11.5);
/// A page's name in its header.
pub const HEADING: Style = Style::semibold(15.0);
/// Page and section titles.
pub const TITLE: Style = Style::semibold(22.0);
/// An icon from the icon font, `size` pixels square.
#[must_use]
pub const fn icon(size: f32) -> Style {
    Style { font: FontId::ICON, size, line_height: 1.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_get_their_own_colour() {
        let colors: Vec<Color> =
            ["Scout", "Newsletter", "Inbound", "Research", "Twitter"].iter().map(|n| agent_color(n, "")).collect();
        for c in &colors {
            assert!(c.iter().all(|v| (0.0..=1.0).contains(v)) && (c[3] - 1.0).abs() < f32::EPSILON, "{c:?}");
        }
        let apart = |a: &Color, b: &Color| a.iter().zip(b).any(|(x, y)| (x - y).abs() > 0.01);
        assert!(colors.windows(2).all(|w| apart(&w[0], &w[1])), "different names, different colours");
        assert_eq!(agent_color("Scout", ""), agent_color("scout!", ""), "folded");
        // A chosen colour is worn as it is; nonsense falls back to the name's.
        let amber = parse_hex("#e8a55e").unwrap();
        assert_eq!(agent_color("Mira", "#e8a55e")[..3], amber);
        assert_eq!(agent_color("Mira", "nope"), agent_color("Mira", ""));
    }

    #[test]
    fn hex_colours_parse_and_round_trip() {
        assert_eq!(parse_hex("#E8A55E").map(to_hex).as_deref(), Some("#e8a55e"));
        assert_eq!(parse_hex(" f00 ").map(to_hex).as_deref(), Some("#ff0000"), "short and bare");
        for bad in ["", "#", "#12345", "#gggggg", "#1234567", "#é12345"] {
            assert!(parse_hex(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn scheme_keys_round_trip() {
        for scheme in Scheme::ALL {
            assert_eq!(Scheme::from_key(Some(scheme.key())), scheme);
        }
        assert_eq!(Scheme::from_key(Some("neon")), Scheme::Black);
        assert_eq!(Scheme::from_key(None), Scheme::Black);
        // Retired schemes land on the nearest one.
        assert_eq!(Scheme::from_key(Some("ember")), Scheme::Black);
        assert_eq!(Scheme::from_key(Some("light")), Scheme::White);
        assert_eq!(Scheme::from_key(Some("parchment")), Scheme::Amber);
    }
}
