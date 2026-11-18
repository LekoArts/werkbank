//! The Inbox's colours: Herdr's own theme, read from its config.toml, so the
//! Inbox looks like the rest of Herdr. Palettes are Herdr's built-in themes as
//! herdr-recap tabulates them (MIT, (c) 2026 Hjortur Stefan Olafsson,
//! github.com/mrolafsson/herdr-recap `themes_herdr.go`).

use ratatui::style::Color;
use std::path::PathBuf;
use std::sync::OnceLock;

/// The tokens the Inbox paints with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    /// Selected row background.
    pub surface: Color,
    /// Secondary text: workspaces, ages, meta lines.
    pub overlay: Color,
    pub subtext: Color,
    pub text: Color,
    pub mauve: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub blue: Color,
    pub teal: Color,
    pub peach: Color,
}

/// The token names of `[theme.custom]`, in `PALETTES` column order.
const TOKENS: [&str; 13] = [
    "accent", "surface0", "overlay0", "overlay1", "text", "subtext0", "mauve", "green", "yellow",
    "red", "blue", "teal", "peach",
];

/// Herdr's built-in themes by canonical name; a colour is `#rrggbb`, an ANSI
/// number, or empty for the terminal's own.
const PALETTES: &[(&str, [&str; 13])] = &[
    (
        "catppuccin",
        [
            "#89b4fa", "#313244", "#6c7086", "#7f849c", "#cdd6f4", "#a6adc8", "#cba6f7", "#a6e3a1",
            "#f9e2af", "#f38ba8", "#89b4fa", "#94e2d5", "#fab387",
        ],
    ),
    (
        "catppuccin-latte",
        [
            "#1e66f5", "#ccd0da", "#9ca0b0", "#8c8fa1", "#4c4f69", "#6c6f85", "#8839ef", "#40a02b",
            "#df8e1d", "#d20f39", "#1e66f5", "#179299", "#fe640b",
        ],
    ),
    (
        "dracula",
        [
            "#bd93f9", "#44475a", "#6272a4", "#828cb4", "#f8f8f2", "#d2d2dc", "#ff79c6", "#50fa7b",
            "#f1fa8c", "#ff5555", "#8be9fd", "#8be9fd", "#ffb86c",
        ],
    ),
    (
        "gruvbox",
        [
            "#d79921", "#3c3836", "#928374", "#a89984", "#ebdbb2", "#d5c4a1", "#d3869b", "#b8bb26",
            "#fabd2f", "#fb4934", "#83a598", "#8ec07c", "#fe8019",
        ],
    ),
    (
        "gruvbox-light",
        [
            "#076678", "#ebdbb2", "#928374", "#7c6f64", "#3c3836", "#504945", "#8f3f71", "#79740e",
            "#b57614", "#9d0006", "#076678", "#427b58", "#af3a03",
        ],
    ),
    (
        "kanagawa",
        [
            "#7e9cd8", "#2a2a37", "#727169", "#87867d", "#dcd7ba", "#c8c3aa", "#957fb8", "#76946a",
            "#c0a36e", "#c34043", "#7e9cd8", "#7fb4ca", "#ffa066",
        ],
    ),
    (
        "kanagawa-lotus",
        [
            "#4d699b", "#dcd5ac", "#a09cac", "#8a8980", "#545464", "#43436c", "#624c83", "#6f894e",
            "#77713f", "#c84053", "#4d699b", "#4e8ca2", "#cc6d00",
        ],
    ),
    (
        "nord",
        [
            "#88c0d0", "#3b4252", "#4c566a", "#646e82", "#eceff4", "#d8dee9", "#b48ead", "#a3be8c",
            "#ebcb8b", "#bf616a", "#81a1c1", "#8fbcbb", "#d08770",
        ],
    ),
    (
        "one-dark",
        [
            "#61afef", "#2c313a", "#5c6370", "#737a87", "#abb2bf", "#969ca8", "#c678dd", "#98c379",
            "#e5c07b", "#e06c75", "#61afef", "#56b6c2", "#d19a66",
        ],
    ),
    (
        "one-light",
        [
            "#4078f2", "#f0f0f1", "#a0a1a7", "#686b77", "#383a42", "#686b77", "#a626a4", "#50a14f",
            "#c18401", "#e45649", "#4078f2", "#0184bc", "#986801",
        ],
    ),
    (
        "rose-pine",
        [
            "#c4a7e7", "#1f1d2e", "#6e6a86", "#908caa", "#e0def4", "#c8c5dc", "#c4a7e7", "#31748f",
            "#f6c177", "#eb6f92", "#31748f", "#9ccfd8", "#ea9a97",
        ],
    ),
    (
        "rose-pine-dawn",
        [
            "#907aa9", "#f2e9e1", "#9893a5", "#797593", "#464261", "#797593", "#907aa9", "#286983",
            "#ea9d34", "#b4637a", "#286983", "#56949f", "#d7827e",
        ],
    ),
    (
        "solarized",
        [
            "#268bd2", "#073642", "#586e75", "#657b83", "#93a1a1", "#839496", "#d33682", "#859900",
            "#b58900", "#dc322f", "#268bd2", "#2aa198", "#cb4b16",
        ],
    ),
    (
        "solarized-light",
        [
            "#268bd2", "#eee8d5", "#93a1a1", "#586e75", "#657b83", "#839496", "#d33682", "#859900",
            "#b58900", "#dc322f", "#268bd2", "#2aa198", "#cb4b16",
        ],
    ),
    (
        "terminal",
        [
            "4", "", "7", "15", "", "7", "7", "2", "3", "9", "4", "6", "3",
        ],
    ),
    (
        "tokyo-night",
        [
            "#7aa2f7", "#24283b", "#565f89", "#697196", "#c0caf5", "#a9b1d6", "#bb9af7", "#9ece6a",
            "#e0af68", "#f7768e", "#7aa2f7", "#7dcfff", "#ff9e64",
        ],
    ),
    (
        "tokyo-night-day",
        [
            "#2e7de9", "#c4c8da", "#8990b3", "#68709a", "#3760bf", "#6172b0", "#7847bd", "#587539",
            "#8c6c3e", "#f52a65", "#2e7de9", "#118c74", "#b15c00",
        ],
    ),
    (
        "vesper",
        [
            "#ffc799", "#232323", "#5c5c5c", "#7e7e7e", "#ffffff", "#a0a0a0", "#ffd1a8", "#99ffe4",
            "#ffc799", "#ff8080", "#b0b0b0", "#66ddcc", "#ffc799",
        ],
    ),
];

/// Herdr's aliases for its theme names (`canonical_theme_name`).
const ALIASES: &[(&str, &str)] = &[
    ("catppuccin-mocha", "catppuccin"),
    ("latte", "catppuccin-latte"),
    ("light", "catppuccin-latte"),
    ("tokyonight", "tokyo-night"),
    ("tokyo-day", "tokyo-night-day"),
    ("tokyonight-day", "tokyo-night-day"),
    ("gruvbox-dark", "gruvbox"),
    ("onedark", "one-dark"),
    ("onelight", "one-light"),
    ("solarized-dark", "solarized"),
    ("lotus", "kanagawa-lotus"),
    ("rosepine", "rose-pine"),
    ("rosepine-dawn", "rose-pine-dawn"),
    ("dawn", "rose-pine-dawn"),
];

/// The theme, read once per Inbox process. Tests always get Catppuccin.
pub fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        if cfg!(test) {
            return from_config("");
        }
        let text = config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        from_config(&text)
    })
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("HERDR_CONFIG_PATH") {
        return Some(p.into());
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    Some(base.join("herdr").join("config.toml"))
}

/// `[theme] name` (default Catppuccin; unknown names fall back to it), then
/// any `[theme.custom]` tokens on top. Only these two tables are read, with
/// a line-based reader: Werkbank has no TOML parser.
pub fn from_config(toml: &str) -> Theme {
    let mut table = String::new();
    let mut name = "catppuccin".to_string();
    let mut custom: Vec<(String, String)> = Vec::new();
    for line in toml.lines() {
        let line = line.split(" #").next().unwrap_or("").trim();
        if let Some(t) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            table = t.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim().trim_matches('"').to_string());
        match table.as_str() {
            "theme" if key == "name" => name = value,
            "theme.custom" => custom.push((key.to_string(), value)),
            _ => {}
        }
    }
    let canonical = canonical(&name);
    let row = PALETTES
        .iter()
        .find(|(n, _)| *n == canonical)
        .or_else(|| PALETTES.iter().find(|(n, _)| *n == "catppuccin"))
        .map(|(_, r)| *r)
        .unwrap_or_default();
    let mut colors = row.map(color);
    for (key, value) in custom {
        if let Some(i) = TOKENS.iter().position(|t| *t == key) {
            colors[i] = color(&value);
        }
    }
    let [accent, surface, overlay, _overlay1, text, subtext, mauve, green, yellow, red, blue, teal, peach] =
        colors;
    Theme {
        accent,
        surface,
        overlay,
        subtext,
        text,
        mauve,
        green,
        yellow,
        red,
        blue,
        teal,
        peach,
    }
}

fn canonical(name: &str) -> String {
    let n = name.to_lowercase().replace([' ', '_'], "-");
    ALIASES
        .iter()
        .find(|(a, _)| *a == n)
        .map_or(n, |(_, c)| c.to_string())
}

/// `#rrggbb`, an ANSI number, or the terminal's own colour.
fn color(s: &str) -> Color {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#').filter(|h| h.len() == 6) {
        if let Ok(v) = u32::from_str_radix(hex, 16) {
            return Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
        }
    }
    match s.parse::<u8>() {
        Ok(n) => Color::Indexed(n),
        Err(_) => Color::Reset,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_config_is_catppuccin() {
        let t = from_config("");
        assert_eq!(t.accent, Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(t.red, Color::Rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(theme(), &t);
    }

    #[test]
    fn a_named_theme_and_its_alias_are_read_from_the_theme_table() {
        let t = from_config("[keys]\nname = \"x\"\n[theme]\nname = \"Tokyo_Night\"\n");
        assert_eq!(t, from_config("[theme]\nname = \"tokyonight\""));
        assert_ne!(t, from_config(""));
        assert_eq!(
            from_config("[theme]\nname = \"no-such\""),
            from_config(""),
            "unknown falls back"
        );
    }

    #[test]
    fn custom_tokens_override_and_the_terminal_theme_uses_ansi() {
        let t = from_config("[theme.custom]\nred = \"#010203\" # mine\n");
        assert_eq!(t.red, Color::Rgb(1, 2, 3));
        assert_eq!(t.blue, from_config("").blue);
        let term = from_config("[theme]\nname = \"terminal\"");
        assert_eq!(term.accent, Color::Indexed(4));
        assert_eq!(term.text, Color::Reset);
    }

    #[test]
    fn every_palette_parses() {
        for (name, row) in PALETTES {
            for c in row.iter().filter(|c| !c.is_empty()) {
                assert_ne!(color(c), Color::Reset, "{name} {c}");
            }
        }
        assert_eq!(PALETTES.len(), 18);
    }
}
