//! A brand kit (decision 0004): a `brand.toml` with the channel's colours,
//! fonts and logos, kept outside this repo. Ghost Run's cards and captions
//! use it when Settings names one (decision 0005's brand-kit title cards).
//! Only what Ghost Run uses is read; everything else in the file is left for
//! the thumbnail renderer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

/// RGB, 0–1.
pub type Rgb = (f64, f64, f64);

#[derive(Debug, Clone, PartialEq)]
pub struct Brand {
    pub name: String,
    pub text: Rgb,
    pub background: Rgb,
    pub accent: Rgb,
    /// Secondary text; the text colour if the kit has none.
    pub neutral: Rgb,
    /// Font files (TrueType or OpenType), if given.
    pub headline: Option<PathBuf>,
    pub label: Option<PathBuf>,
    /// The full logo (SVG), for title cards.
    pub logo: Option<PathBuf>,
    /// The small logo (SVG), for end cards.
    pub logo_small: Option<PathBuf>,
}

#[derive(Deserialize)]
struct File {
    version: u32,
    #[serde(default)]
    name: String,
    colors: Colors,
    #[serde(default)]
    fonts: Fonts,
    #[serde(default)]
    logo: Logo,
}

#[derive(Deserialize)]
struct Colors {
    text: String,
    background: String,
    accent: String,
    neutral: Option<String>,
}

#[derive(Deserialize, Default)]
struct Fonts {
    headline: Option<String>,
    label: Option<String>,
}

#[derive(Deserialize, Default)]
struct Logo {
    full: Option<String>,
    small: Option<String>,
}

impl Brand {
    /// Reads a `brand.toml`; its paths are relative to it, and files that
    /// aren't there are left out rather than failing.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text, path.parent().unwrap_or(Path::new(".")))
    }

    fn parse(text: &str, dir: &Path) -> Result<Self> {
        let f: File = toml::from_str(text)?;
        ensure!(
            f.version == 1,
            "brand kit format {} isn't one Ghost Run knows (1)",
            f.version
        );
        let file = |p: Option<String>| p.map(|p| dir.join(p)).filter(|p| p.is_file());
        let text = colour(&f.colors.text)?;
        Ok(Self {
            name: f.name,
            text,
            background: colour(&f.colors.background)?,
            accent: colour(&f.colors.accent)?,
            neutral: f
                .colors
                .neutral
                .as_deref()
                .map(colour)
                .transpose()?
                .unwrap_or(text),
            headline: file(f.fonts.headline),
            label: file(f.fonts.label),
            logo: file(f.logo.full),
            logo_small: file(f.logo.small),
        })
    }
}

/// `#rrggbb` as RGB, 0–1.
pub fn colour(hex: &str) -> Result<Rgb> {
    let h = hex.trim().trim_start_matches('#');
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("{hex} isn't a #rrggbb colour");
    }
    let byte = |i: usize| f64::from(u8::from_str_radix(&h[i..i + 2], 16).expect("checked")) / 255.0;
    Ok((byte(0), byte(2), byte(4)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_colours_fonts_and_logos_relative_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("fonts")).unwrap();
        std::fs::write(dir.path().join("fonts/Head.ttf"), "x").unwrap();
        std::fs::write(dir.path().join("logo.svg"), "<svg/>").unwrap();
        let toml = r##"
            version = 1
            name = "Trail Co"
            [colors]
            text = "#e6e1d6"
            background = "#1d1b18"
            accent = "#8A9A5B"
            [fonts]
            headline = "fonts/Head.ttf"
            label = "fonts/Missing.ttf"
            [logo]
            full = "logo.svg"
            [thumbnail]
            width = 1920
        "##;
        std::fs::write(dir.path().join("brand.toml"), toml).unwrap();
        let b = Brand::load(&dir.path().join("brand.toml")).unwrap();
        assert_eq!(b.name, "Trail Co");
        assert_eq!(b.background, (29.0 / 255.0, 27.0 / 255.0, 24.0 / 255.0));
        assert_eq!(b.accent.0, 138.0 / 255.0);
        assert_eq!(b.neutral, b.text);
        assert_eq!(b.headline, Some(dir.path().join("fonts/Head.ttf")));
        assert_eq!(b.label, None);
        assert_eq!(b.logo, Some(dir.path().join("logo.svg")));
        assert!(Brand::parse(&toml.replace("version = 1", "version = 2"), dir.path()).is_err());
        assert!(colour("#12345").is_err() && colour("zzzzzz").is_err());
    }
}
