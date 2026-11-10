use crate::domain::Selection;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Create,
    Rename,
    Close,
}
pub(super) struct Dialog {
    pub(super) operation: Operation,
    pub(super) selection: Selection,
    pub(super) name: String,
    pub(super) text: String,
    pub(super) direction: &'static str,
    pub(super) pasting: bool,
}
impl Dialog {
    /// Some(true) commits; Some(false) cancels. Close defaults to cancellation.
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Option<bool> {
        if bytes == b"\x1b[200~" {
            self.pasting = true;
            return None;
        }
        if bytes == b"\x1b[201~" {
            self.pasting = false;
            return None;
        }
        if self.pasting && self.operation == Operation::Close {
            return None;
        }
        if self.operation == Operation::Close {
            return match bytes {
                b"y" | b"Y" => Some(true),
                b"n" | b"N" | b"\r" | b"\n" | b"\x1b" | b"\x03" => Some(false),
                _ => None,
            };
        }
        match bytes {
            b"\x1b" | b"\x03" => Some(false),
            b"\r" | b"\n" => Some(true),
            b"\x7f" | b"\x08" => {
                self.text.pop();
                None
            }
            b"\x15" => {
                self.text.clear();
                None
            }
            _ => {
                if let Ok(text) = std::str::from_utf8(bytes)
                    && !text.chars().any(char::is_control)
                    && self.text.chars().count() + text.chars().count() <= 128
                {
                    self.text.push_str(text);
                }
                None
            }
        }
    }
}
