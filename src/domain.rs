//! Shared identifiers, independent of UI and transport.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub session: String,
    pub block: String,
}

#[derive(Clone, Debug)]
pub enum Selection {
    Session(String),
    Window { session: String, window: String },
    Block(Target),
}
impl Selection {
    pub fn session(&self) -> &str {
        match self {
            Self::Session(s) | Self::Window { session: s, .. } => s,
            Self::Block(t) => &t.session,
        }
    }
    pub fn kind(&self) -> &str {
        match self {
            Self::Session(_) => "session",
            Self::Window { .. } => "tab",
            Self::Block(_) => "block",
        }
    }
}
