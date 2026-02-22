use anyhow::{Context, Result, anyhow};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const APP_DIR: &str = "rtsp-tui";

#[derive(Debug, Clone, Copy)]
pub struct ThemePalette {
    pub text: Color,
    pub muted: Color,
    pub border: Color,
    pub border_active: Color,
    pub accent: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
}

impl Default for ThemePalette {
    fn default() -> Self {
        Self {
            text: Color::Rgb(231, 235, 243),
            muted: Color::Rgb(145, 152, 170),
            border: Color::Rgb(88, 98, 120),
            border_active: Color::Rgb(114, 140, 255),
            accent: Color::Rgb(102, 216, 255),
            success: Color::Rgb(103, 212, 142),
            warning: Color::Rgb(255, 198, 109),
            error: Color::Rgb(255, 121, 134),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct ThemeFile {
    text: String,
    muted: String,
    border: String,
    border_active: String,
    accent: String,
    success: String,
    warning: String,
    error: String,
}

impl Default for ThemeFile {
    fn default() -> Self {
        Self {
            text: "#E7EBF3".to_owned(),
            muted: "#9198AA".to_owned(),
            border: "#586278".to_owned(),
            border_active: "#728CFF".to_owned(),
            accent: "#66D8FF".to_owned(),
            success: "#67D48E".to_owned(),
            warning: "#FFC66D".to_owned(),
            error: "#FF7986".to_owned(),
        }
    }
}

impl ThemePalette {
    fn from_file(file: &ThemeFile) -> Result<Self> {
        Ok(Self {
            text: parse_hex_color("text", &file.text)?,
            muted: parse_hex_color("muted", &file.muted)?,
            border: parse_hex_color("border", &file.border)?,
            border_active: parse_hex_color("border_active", &file.border_active)?,
            accent: parse_hex_color("accent", &file.accent)?,
            success: parse_hex_color("success", &file.success)?,
            warning: parse_hex_color("warning", &file.warning)?,
            error: parse_hex_color("error", &file.error)?,
        })
    }
}

pub fn theme_path() -> Result<PathBuf> {
    scoped_path(APP_DIR)
}

pub fn load_or_create_theme() -> Result<ThemePalette> {
    let path = theme_path()?;

    if !path.exists() {
        let default_file = ThemeFile::default();
        write_theme_file(&path, &default_file)?;
        return ThemePalette::from_file(&default_file);
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed reading theme config at {}", path.display()))?;
    let parsed = serde_json::from_str::<ThemeFile>(&raw)
        .with_context(|| format!("failed parsing theme config at {}", path.display()))?;
    ThemePalette::from_file(&parsed)
}

fn write_theme_file(path: &Path, theme: &ThemeFile) -> Result<()> {
    ensure_parent_dir(path)?;
    let payload = serde_json::to_string_pretty(theme).context("failed serializing theme config")?;
    fs::write(path, payload)
        .with_context(|| format!("failed writing theme config at {}", path.display()))?;
    Ok(())
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating theme directory {}", parent.display()))?;
    }
    Ok(())
}

fn parse_hex_color(key: &str, value: &str) -> Result<Color> {
    let input = value.trim();
    let hex = input.strip_prefix('#').unwrap_or(input);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "theme field '{key}' must be a hex color like #RRGGBB, got '{value}'"
        ));
    }

    let red = u8::from_str_radix(&hex[0..2], 16)
        .with_context(|| format!("theme field '{key}' has invalid red component"))?;
    let green = u8::from_str_radix(&hex[2..4], 16)
        .with_context(|| format!("theme field '{key}' has invalid green component"))?;
    let blue = u8::from_str_radix(&hex[4..6], 16)
        .with_context(|| format!("theme field '{key}' has invalid blue component"))?;

    Ok(Color::Rgb(red, green, blue))
}

fn scoped_path(app_dir: &str) -> Result<PathBuf> {
    let root = dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .context("unable to determine user data directory")?;
    Ok(root.join(app_dir).join("theme.json"))
}
