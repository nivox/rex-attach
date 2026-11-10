//! Configurable local actions. Input remains a byte stream; unbound terminal keys
//! and bracketed pastes are forwarded unchanged, including fragmented sequences.
use crate::action::Action;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};

pub fn defaults() -> Value {
    json!({"shortcuts": {
        "browser": {
            "up":"client.browser.up", "k":"client.browser.up", "down":"client.browser.down", "j":"client.browser.down",
            "left":"client.browser.left", "h":"client.browser.left", "right":"client.browser.right", "l":"client.browser.right",
            "enter":"client.browser.choose", "esc":"client.browser.exit", "q":"client.browser.exit", "ctrl+c":"client.browser.exit",
            "?":"client.shortcuts", "p":"client.browser.toggle_preview", "c":"client.create", "x":"client.close", "r":"client.rename", "ctrl+r":"session.refresh"
        },
        "terminal": {
            "ctrl+b>ctrl+shift+'":"pane.split.down", "ctrl+b>ctrl+shift+5":"pane.split.right",
            "ctrl+b>\"":"pane.split.down", "ctrl+b>%":"pane.split.right",
            "ctrl+b>?":"client.shortcuts", "ctrl+b>o":"pane.focus_next", "ctrl+b>c":"client.tab.new", "ctrl+b>n":"client.tab.next",
            "ctrl+b>p":"client.tab.previous", "ctrl+b>d":"client.detach", "ctrl+b>ctrl+b":"client.send_prefix"
        }
    }})
}

#[derive(Clone, Default)]
pub struct Bindings {
    encoded: Vec<(Vec<u8>, Action)>,
    pub display: Vec<(String, Action)>,
}
pub struct Config {
    pub preview_layout: crate::browser::PreviewLayout,
    pub browser: Bindings,
    pub terminal: Bindings,
}
impl Bindings {
    pub fn shortcut(&self, action: &str) -> String {
        let keys: Vec<_> = self
            .display
            .iter()
            .filter(|(_, a)| a == action)
            .map(|(key, _)| {
                key.split('>')
                    .map(|chord| {
                        if chord.contains('+') {
                            chord
                                .split('+')
                                .map(|part| match part {
                                    "ctrl" => "Ctrl".into(),
                                    "alt" => "Alt".into(),
                                    "shift" => "Shift".into(),
                                    other => other.to_uppercase(),
                                })
                                .collect::<Vec<_>>()
                                .join("-")
                        } else {
                            chord.to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        if keys.is_empty() {
            "unbound".into()
        } else {
            keys.join(" / ")
        }
    }
}
impl Config {
    pub fn load() -> Result<Self> {
        let path = path()?;
        let value = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(value).with_context(|| format!("invalid configuration in {}", path.display()))
    }
    pub(crate) fn parse(value: Value) -> Result<Self> {
        ensure!(
            value.is_null() || value.is_object(),
            "config must be an object"
        );
        if let Some(shortcuts) = value.get("shortcuts") {
            ensure!(shortcuts.is_object(), "shortcuts must be an object");
            for key in shortcuts.as_object().unwrap().keys() {
                ensure!(
                    matches!(key.as_str(), "browser" | "terminal"),
                    "unknown shortcuts section: {key}"
                );
            }
        }
        Ok(Self {
            preview_layout: value
                .get("preview_layout")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .context("preview_layout must be column or row")?
                .unwrap_or_default(),
            browser: build("browser", &value)?,
            terminal: build("terminal", &value)?,
        })
    }
}
fn build(section: &str, value: &Value) -> Result<Bindings> {
    let default = defaults();

    let mut map: BTreeMap<String, Value> = default["shortcuts"][section]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(overrides) = value.get("shortcuts").and_then(|v| v.get(section)) {
        for (key, action) in overrides
            .as_object()
            .context("shortcut section must be an object")?
        {
            map.insert(key.clone(), action.clone());
        }
    }
    let mut bindings = BTreeMap::new();
    let mut display = Vec::new();
    for (key, action) in map {
        let sequences = sequences(&key).with_context(|| format!("shortcut {key:?}"))?;
        if action.is_null() {
            continue;
        }
        let action = action
            .as_str()
            .context("shortcut action must be a string or null")?;
        let action: Action = action.parse()?;
        ensure!(
            action.allowed(section == "terminal"),
            "unsupported {section} action: {action}"
        );
        display.push((key.clone(), action));
        for bytes in sequences {
            if let Some(old) = bindings.insert(bytes, action) {
                ensure!(old == action, "conflicting shortcut encodings for {key}");
            }
        }
    }
    Ok(Bindings {
        encoded: bindings.into_iter().collect(),
        display,
    })
}
pub fn path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("HOME or XDG_CONFIG_DIR must be set")?;
    Ok(base.join("rex-attach/config.json"))
}
fn sequences(key: &str) -> Result<Vec<Vec<u8>>> {
    let mut result = vec![vec![]];
    for part in key.split('>') {
        let variants = chord(part.trim())?;
        result = result
            .into_iter()
            .flat_map(|prefix| {
                variants
                    .iter()
                    .map(move |suffix| [prefix.as_slice(), suffix].concat())
            })
            .collect();
    }
    Ok(result)
}
fn chord(key: &str) -> Result<Vec<Vec<u8>>> {
    let named = named_key(key);
    if let Some(v) = named {
        return Ok(v);
    }
    if key.chars().count() == 1 {
        return Ok(vec![key.as_bytes().to_vec()]);
    }
    modified_key(key)
}
fn named_key(key: &str) -> Option<Vec<Vec<u8>>> {
    match key.to_ascii_lowercase().as_str() {
        "up" => Some(vec![b"\x1b[A".to_vec(), b"\x1bOA".to_vec()]),
        "down" => Some(vec![b"\x1b[B".to_vec(), b"\x1bOB".to_vec()]),
        "right" => Some(vec![b"\x1b[C".to_vec(), b"\x1bOC".to_vec()]),
        "left" => Some(vec![b"\x1b[D".to_vec(), b"\x1bOD".to_vec()]),
        "enter" => Some(vec![vec![13], vec![10]]),
        "esc" | "escape" => Some(vec![vec![27]]),
        "tab" => Some(vec![vec![9]]),
        "space" => Some(vec![vec![32]]),
        _ => None,
    }
}
fn modified_key(key: &str) -> Result<Vec<Vec<u8>>> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;
    let parts: Vec<_> = key.split('+').collect();
    for modifier in &parts[..parts.len() - 1] {
        match modifier.to_ascii_lowercase().as_str() {
            "ctrl" => ctrl = true,
            "shift" => shift = true,
            "alt" => alt = true,
            _ => bail!("unknown modifier {modifier}"),
        }
    }
    let letter = parts.last().unwrap();
    ensure!(letter.chars().count() == 1, "unknown key {key}");
    let c = letter.chars().next().unwrap();
    let mods = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let mut variants = vec![
        format!("\x1b[{};{mods}u", c as u32).into_bytes(),
        format!("\x1b[27;{mods};{}~", c as u32).into_bytes(),
    ];
    if ctrl && !shift && c.is_ascii() && (b'@'..=b'_').contains(&(c.to_ascii_uppercase() as u8)) {
        let mut bytes = vec![(c.to_ascii_uppercase() as u8) & 31];
        if alt {
            bytes.insert(0, 27);
        }
        variants.push(bytes);
    } else if !ctrl && c.is_ascii() {
        let mut bytes = vec![if shift {
            c.to_ascii_uppercase() as u8
        } else {
            c as u8
        }];
        if alt {
            bytes.insert(0, 27);
        }
        variants.push(bytes);
    }
    // Terminals may report the shifted symbol rather than the physical key.
    if ctrl && shift && matches!(c, '\'' | '5') {
        let shifted = if c == '\'' { '"' } else { '%' };
        for modifier in [5, 6] {
            variants.push(format!("\x1b[{};{modifier}u", shifted as u32).into_bytes());
            variants.push(format!("\x1b[27;{modifier};{}~", shifted as u32).into_bytes());
        }
    }
    Ok(variants)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    Raw(Vec<u8>),
    Action(Action, Vec<u8>),
}
impl Event {
    pub fn bytes(self) -> Vec<u8> {
        match self {
            Self::Raw(b) | Self::Action(_, b) => b,
        }
    }
}
pub struct Matcher {
    bindings: Bindings,
    buffer: Vec<u8>,
    pending: Vec<u8>,
    paste: bool,
}
impl Matcher {
    pub fn new(bindings: &Bindings) -> Self {
        Self {
            bindings: bindings.clone(),
            buffer: vec![],
            pending: vec![],
            paste: false,
        }
    }
    pub fn remaining(&mut self) -> Vec<u8> {
        let mut rest = std::mem::take(&mut self.pending);
        rest.append(&mut self.buffer);
        rest
    }
    pub fn delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(if self.buffer.is_empty() { 1000 } else { 80 })
    }
    pub fn waiting(&self) -> bool {
        !self.buffer.is_empty() || !self.pending.is_empty()
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buffer.extend_from_slice(bytes);
        let mut events = vec![];
        while let Some(n) = token_len(&self.buffer) {
            let token: Vec<_> = self.buffer.drain(..n).collect();
            if token == b"\x1b[200~" {
                self.flush(&mut events);
                self.paste = true;
            }
            if self.paste {
                if token == b"\x1b[201~" {
                    self.paste = false;
                }
                events.push(Event::Raw(token));
                continue;
            }
            self.pending.extend_from_slice(&token);
            let exact = self
                .bindings
                .encoded
                .iter()
                .find(|(key, _)| *key == self.pending);
            let longer =
                self.bindings.encoded.iter().any(|(key, _)| {
                    key.len() > self.pending.len() && key.starts_with(&self.pending)
                });
            if !longer {
                if let Some((_, action)) = exact {
                    events.push(Event::Action(*action, std::mem::take(&mut self.pending)));
                } else {
                    self.flush(&mut events);
                }
            }
        }
        events
    }
    fn flush(&mut self, events: &mut Vec<Event>) {
        if !self.pending.is_empty() {
            events.push(Event::Raw(std::mem::take(&mut self.pending)));
        }
    }
    pub fn timeout(&mut self) -> Vec<Event> {
        self.pending.append(&mut self.buffer);
        let mut events = vec![];
        if let Some((_, action)) = self
            .bindings
            .encoded
            .iter()
            .find(|(key, _)| *key == self.pending)
        {
            events.push(Event::Action(*action, std::mem::take(&mut self.pending)));
        } else {
            self.flush(&mut events);
        }
        events
    }
}
fn token_len(bytes: &[u8]) -> Option<usize> {
    let &first = bytes.first()?;
    if first == 27 {
        let &second = bytes.get(1)?;
        if second == b'[' {
            return bytes
                .iter()
                .enumerate()
                .skip(2)
                .find(|(_, b)| (0x40..=0x7e).contains(*b))
                .map(|(i, _)| i + 1);
        }
        if second == b'O' {
            return (bytes.len() >= 3).then_some(3);
        }
        return Some(2);
    }
    let n = if first < 128 {
        1
    } else if first < 224 {
        2
    } else if first < 240 {
        3
    } else {
        4
    };
    (bytes.len() >= n).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_quits_with_q_and_terminal_uses_one_prefix() {
        let config = Config::parse(Value::Null).unwrap();
        assert_eq!(
            Matcher::new(&config.browser).feed(b"q"),
            vec![Event::Action("client.browser.exit".into(), b"q".to_vec())]
        );
        assert_eq!(
            Matcher::new(&config.terminal).feed(b"\x1dd"),
            vec![Event::Raw(vec![0x1d]), Event::Raw(b"d".to_vec())]
        );
        assert_eq!(config.terminal.shortcut("client.detach"), "Ctrl-B d");
        assert_eq!(config.terminal.shortcut("client.shortcuts"), "Ctrl-B ?");
        assert!(
            Config::parse(
                json!({"shortcuts":{"terminal":{"ctrl+]>ctrl+]":"client.send_detach_prefix"}}})
            )
            .is_err()
        );
    }
    #[test]
    fn config_overrides_unbinds_and_rejects_invalid_actions() {
        assert_eq!(
            Config::parse(json!({"preview_layout":"row"}))
                .unwrap()
                .preview_layout,
            crate::browser::PreviewLayout::Row
        );
        assert_eq!(
            Config::parse(json!({"preview_layout":"column"}))
                .unwrap()
                .preview_layout,
            crate::browser::PreviewLayout::Column
        );
        for value in [Value::Null, json!({})] {
            assert_eq!(
                Config::parse(value).unwrap().preview_layout,
                crate::browser::PreviewLayout::Row
            );
        }
        for invalid in [json!("bottom"), json!(false), Value::Null] {
            assert!(Config::parse(json!({"preview_layout":invalid})).is_err());
        }
        let config =
            Config::parse(json!({"shortcuts":{"browser":{"r":null,"z":"client.rename"}}})).unwrap();
        assert!(
            config
                .browser
                .display
                .contains(&("z".into(), "client.rename".into()))
        );
        assert!(!config.browser.display.iter().any(|(key, _)| key == "r"));
        let mut m = Matcher::new(&config.browser);
        assert_eq!(
            m.feed(b"rz"),
            vec![
                Event::Raw(b"r".to_vec()),
                Event::Action("client.rename".into(), b"z".to_vec())
            ]
        );
        assert!(Config::parse(json!({"shortcuts":{"terminal":{"x":"pane.close"}}})).is_err());
        assert!(Config::parse(json!({"shortcuts":{"browser":{"badkey":"client.close"}}})).is_err());
    }
    #[test]
    fn fragmented_prefixes_modified_keys_and_passthrough() {
        let config = Config::parse(Value::Null).unwrap();
        let mut m = Matcher::new(&config.terminal);
        assert!(m.feed(b"\x02\x1b[53;").is_empty());
        assert_eq!(
            m.feed(b"6u"),
            vec![Event::Action(
                "pane.split.right".into(),
                b"\x02\x1b[53;6u".to_vec()
            )]
        );
        assert_eq!(m.feed(b"\x02z"), vec![Event::Raw(b"\x02z".to_vec())]);
        assert!(m.feed(b"\x02").is_empty());
        assert_eq!(m.timeout(), vec![Event::Raw(vec![2])]);
        assert_eq!(m.feed(b"\x1b[A"), vec![Event::Raw(b"\x1b[A".to_vec())]);
    }
    #[test]
    fn paste_does_not_invoke_actions_and_escape_waits() {
        let config = Config::parse(Value::Null).unwrap();
        let mut m = Matcher::new(&config.browser);
        let events = m.feed(b"\x1b[200~cxr\x1b[201~");
        assert!(events.iter().all(|e| matches!(e, Event::Raw(_))));
        assert!(m.feed(b"\x1b").is_empty());
        assert!(matches!(&m.timeout()[0],Event::Action(a,_) if a=="client.browser.exit"));
    }
}
