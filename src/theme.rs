use crate::vt::{
    Terminal,
    style::{Palette, RgbColor},
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt::Write};

#[derive(Deserialize, Serialize)]
pub struct Theme {
    #[serde(skip_serializing_if = "Option::is_none")]
    foreground: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    palette: Option<Vec<String>>,
}

impl Theme {
    pub fn apply(&self, terminal: &mut Terminal) -> Result<()> {
        let mut palette = Palette::default();
        for entry in self.palette.iter().flatten() {
            if let Ok((index, color)) = Palette::parse_palette_entry(entry) {
                palette.set(index, color);
            }
        }
        let parse = |value: Option<&str>| value.and_then(|value| RgbColor::parse(value).ok());
        // Ghostty updates the rendered defaults only when both colors are set.
        let foreground = parse(self.foreground.as_deref()).unwrap_or(RgbColor {
            r: 255,
            g: 255,
            b: 255,
        });
        let background = parse(self.background.as_deref()).unwrap_or_default();
        terminal.set_theme(
            foreground,
            background,
            parse(self.cursor.as_deref()),
            palette,
        )
    }
}

pub fn append_queries(query: &mut String) {
    query.push_str("\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]12;?\x1b\\");
    for index in 0..=u8::MAX {
        write!(query, "\x1b]4;{index};?\x1b\\").unwrap();
    }
}

pub fn reply_prefix(body: &[u8]) -> bool {
    [b"4;".as_slice(), b"10;", b"11;", b"12;"]
        .iter()
        .any(|prefix| prefix.starts_with(body) || body.starts_with(prefix))
}

#[derive(Default)]
pub struct Probe {
    foreground: Option<String>,
    background: Option<String>,
    cursor: Option<String>,
    palette: BTreeMap<u8, String>,
}

impl Probe {
    pub fn reply(&mut self, body: &[u8]) -> bool {
        let Ok(body) = std::str::from_utf8(body) else {
            return false;
        };
        let Some((command, value)) = body.split_once(';') else {
            return false;
        };
        if command == "4" {
            let Some((index, color)) = value.split_once(';') else {
                return false;
            };
            let (Ok(index), Some(color)) = (index.parse(), normalize(color)) else {
                return false;
            };
            self.palette.insert(index, color);
            return true;
        }
        let field = match command {
            "10" => &mut self.foreground,
            "11" => &mut self.background,
            "12" => &mut self.cursor,
            _ => return false,
        };
        let Some(color) = normalize(value) else {
            return false;
        };
        *field = Some(color);
        true
    }

    pub fn finish(self) -> Option<Theme> {
        // A partial set of defaults could make the remote text unreadable.
        Some(Theme {
            foreground: Some(self.foreground?),
            background: Some(self.background?),
            cursor: self.cursor,
            palette: Some(
                self.palette
                    .into_iter()
                    .map(|(index, color)| format!("{index}={color}"))
                    .collect(),
            ),
        })
    }
}

fn normalize(value: &str) -> Option<String> {
    if !value.starts_with("rgb:") && !value.starts_with('#') {
        return None;
    }
    let RgbColor { r, g, b } = RgbColor::parse(value).ok()?;
    Some(format!("#{r:02x}{g:02x}{b:02x}"))
}
