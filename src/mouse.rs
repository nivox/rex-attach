//! Outer-terminal mouse reports, kept separate from keyboard shortcuts.
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub code: u16,
    pub x: u16,
    pub y: u16,
    pub release: bool,
}

impl Mouse {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let (code, x, y, release) = if let Some(body) = bytes.strip_prefix(b"\x1b[<") {
            let (&end, body) = body.split_last()?;
            if !matches!(end, b'M' | b'm') {
                return None;
            }
            let mut fields = std::str::from_utf8(body).ok()?.split(';');
            let code = fields.next()?.parse().ok()?;
            let x: u16 = fields.next()?.parse().ok()?;
            let y: u16 = fields.next()?.parse().ok()?;
            if fields.next().is_some() {
                return None;
            }
            (code, x.checked_sub(1)?, y.checked_sub(1)?, end == b'm')
        } else if bytes.starts_with(b"\x1b[M") && bytes.len() == 6 {
            let code = u16::from(bytes[3].checked_sub(32)?);
            (
                code,
                u16::from(bytes[4].checked_sub(33)?),
                u16::from(bytes[5].checked_sub(33)?),
                code & 3 == 3 && code & 64 == 0,
            )
        } else {
            return None;
        };
        (code < 128 && !(code & 64 != 0 && release)).then_some(Self {
            code,
            x,
            y,
            release,
        })
    }

    pub fn wheel(self) -> Option<(i32, i32)> {
        if self.code & 64 == 0 || self.release {
            return None;
        }
        Some(match self.code & 3 {
            0 => (0, -1),
            1 => (0, 1),
            2 => (-1, 0),
            _ => (1, 0),
        })
    }

    pub fn left_press(self) -> bool {
        !self.release && self.code & (3 | 32 | 64 | 128) == 0
    }

    /// Rex encodes this event using the application's current mouse protocol.
    pub fn event(self) -> Value {
        if let Some((dx, dy)) = self.wheel() {
            return json!({"type":"wheel", "payload":{"x":self.x,"y":self.y,"delta_x":dx,"delta_y":dy}});
        }
        let action = if self.release {
            "release"
        } else if self.code & 32 != 0 {
            "motion"
        } else {
            "press"
        };
        let button = match (self.code & 128 != 0, self.code & 3) {
            (false, 0) => Some("left"),
            (false, 1) => Some("middle"),
            (false, 2) => Some("right"),
            _ => None,
        };
        let mods = [(4, "shift"), (8, "alt"), (16, "ctrl")]
            .into_iter()
            .filter_map(|(bit, name)| (self.code & bit != 0).then_some(name))
            .collect::<Vec<_>>()
            .join("+");
        let mut payload = json!({"action":action,"x":self.x,"y":self.y,"mods":mods});
        if let Some(button) = button {
            payload["button"] = json!(button);
        }
        json!({"type":"mouse", "payload":payload})
    }
}

/// Successful presses on this attachment only. Releases are sent before opening
/// a local modal or switching targets, so remote drag state cannot leak.
#[derive(Default)]
pub struct Buttons([Option<Mouse>; 3]);
impl Buttons {
    pub fn record(&mut self, mouse: Mouse) {
        if mouse.wheel().is_some() {
            return;
        }
        let button = usize::from(mouse.code & 3);
        if mouse.release {
            if button < 3 {
                self.0[button] = None;
            } else {
                self.0.fill(None);
            }
        } else if mouse.code & 32 == 0 && button < 3 {
            self.0[button] = Some(mouse);
        } else {
            for pressed in self.0.iter_mut().flatten() {
                pressed.x = mouse.x;
                pressed.y = mouse.y;
            }
        }
    }
    pub fn releases(&mut self) -> Vec<Mouse> {
        self.0
            .iter_mut()
            .filter_map(Option::take)
            .map(|mut m| {
                m.release = true;
                m
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_button_state_releases_once_at_the_last_position() {
        let mut buttons = Buttons::default();
        buttons.record(Mouse::parse(b"\x1b[<0;10;5M").unwrap());
        buttons.record(Mouse::parse(b"\x1b[<32;11;6M").unwrap());
        let releases = buttons.releases();
        assert_eq!(releases, vec![Mouse::parse(b"\x1b[<0;11;6m").unwrap()]);
        assert!(buttons.releases().is_empty());
    }

    #[test]
    fn decodes_buttons_modifiers_wheel_and_large_cell_coordinates() {
        let mouse = Mouse::parse(b"\x1b[<20;120;37m").unwrap();
        assert_eq!(
            mouse.event(),
            json!({"type":"mouse","payload":{"action":"release","button":"left","mods":"shift+ctrl","x":119,"y":36}})
        );
        assert_eq!(
            Mouse::parse(b"\x1b[<64;1;1M").unwrap().wheel(),
            Some((0, -1))
        );
        assert_eq!(
            Mouse::parse(b"\x1b[<67;1;1M").unwrap().wheel(),
            Some((1, 0))
        );
        assert_eq!(
            Mouse::parse(b"\x1b[<35;1;1M").unwrap().event()["payload"]["action"],
            "motion"
        );
        for invalid in [
            b"\x1b[<0;0;1M".as_slice(),
            b"\x1b[<0;65536;1M",
            b"\x1b[<0;1;1;2M",
            b"\x1b[<0;1;1x",
        ] {
            assert!(Mouse::parse(invalid).is_none());
        }
    }
}
