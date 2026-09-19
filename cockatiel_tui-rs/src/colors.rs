use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::style::Color;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ColorFile {
    #[serde(default)]
    borders: HashMap<String, String>,
    #[serde(default)]
    status: HashMap<String, String>,
    #[serde(default)]
    platforms: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ColorConfig {
    pub borders: HashMap<String, Color>,
    pub status: HashMap<String, Color>,
    pub platforms: HashMap<String, Color>,
}

fn parse_color(s: &str) -> Color {
    match s.to_lowercase().as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "orange" => Color::Indexed(208),
        "pink" => Color::Indexed(205),
        "brown" => Color::Indexed(130),
        _ => {
            if let Ok(n) = s.parse::<u8>() {
                Color::Indexed(n)
            } else {
                Color::White
            }
        }
    }
}

pub fn load_colors(path: &PathBuf) -> ColorConfig {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return default_colors(),
    };

    let file: ColorFile = match serde_json::from_str(&content) {
        Ok(f) => f,
        Err(_) => return default_colors(),
    };

    let borders: HashMap<String, Color> = file.borders.iter()
        .map(|(k, v)| (k.clone(), parse_color(v)))
        .collect();

    let status: HashMap<String, Color> = file.status.iter()
        .map(|(k, v)| (k.clone(), parse_color(v)))
        .collect();

    let platforms: HashMap<String, Color> = file.platforms.iter()
        .map(|(k, v)| (k.clone(), parse_color(v)))
        .collect();

    ColorConfig { borders, status, platforms }
}

pub fn default_colors() -> ColorConfig {
    let mut borders = HashMap::new();
    borders.insert("inactive".to_string(), Color::DarkGray);
    borders.insert("logo".to_string(), Color::Indexed(208));
    borders.insert("log".to_string(), Color::Yellow);
    borders.insert("db_platforms".to_string(), Color::Magenta);
    borders.insert("modules".to_string(), Color::Cyan);
    borders.insert("hotkey_bar".to_string(), Color::Green);
    borders.insert("chart".to_string(), Color::Blue);
    borders.insert("prompts".to_string(), Color::Magenta);

    let mut status = HashMap::new();
    status.insert("online".to_string(), Color::Green);
    status.insert("connected".to_string(), Color::Green);
    status.insert("starting".to_string(), Color::Yellow);
    status.insert("offline".to_string(), Color::DarkGray);
    status.insert("stopped".to_string(), Color::Yellow);
    status.insert("disconnected".to_string(), Color::Yellow);
    status.insert("error".to_string(), Color::LightRed);
    status.insert("crashed".to_string(), Color::Red);

    let mut platforms = HashMap::new();
    platforms.insert("twitch".to_string(), Color::Magenta);
    platforms.insert("youtube".to_string(), Color::Red);
    platforms.insert("kick".to_string(), Color::Green);
    platforms.insert("discord".to_string(), Color::Blue);

    ColorConfig { borders, status, platforms }
}

impl ColorConfig {
    pub fn border_color(&self, window_name: &str) -> Color {
        if let Some(c) = self.borders.get(window_name) {
            *c
        } else {
            Color::DarkGray
        }
    }

    pub fn active_border_color(&self, window_name: &str) -> Color {
        if let Some(c) = self.borders.get(window_name) {
            *c
        } else {
            Color::Gray
        }
    }

    pub fn status_color(&self, status: &str) -> Color {
        if let Some(c) = self.status.get(status) {
            *c
        } else {
            Color::White
        }
    }

    pub fn platform_color(&self, platform: &str) -> Color {
        if let Some(c) = self.platforms.get(platform) {
            *c
        } else {
            Color::White
        }
    }
}
