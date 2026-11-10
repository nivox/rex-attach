//! Public action names, descriptions and allowed contexts have one definition.
use anyhow::{Result, bail};
macro_rules! actions {
    ($($variant:ident => ($name:literal, $description:literal, $browser:literal, $terminal:literal)),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Action { $($variant),* }
        impl Action {
            pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $name),* } }
            pub fn description(self) -> &'static str { match self { $(Self::$variant => $description),* } }
            pub fn allowed(self, terminal: bool) -> bool { match self { $(Self::$variant => if terminal { $terminal } else { $browser }),* } }
        }
        impl std::str::FromStr for Action {
            type Err = anyhow::Error;
            fn from_str(name: &str) -> Result<Self> { match name { $($name => Ok(Self::$variant)),*, _ => bail!("unsupported action: {name}") } }
        }
    }
}
actions! {
    ClientShortcuts => ("client.shortcuts", "Show shortcut help", true, true),
    ClientBrowserUp => ("client.browser.up", "Previous entry", true, false),
    ClientBrowserDown => ("client.browser.down", "Next entry", true, false),
    ClientBrowserLeft => ("client.browser.left", "Parent column", true, false),
    ClientBrowserRight => ("client.browser.right", "Child column", true, false),
    ClientBrowserChoose => ("client.browser.choose", "Choose / attach", true, false),
    ClientBrowserTogglePreview => ("client.browser.toggle_preview", "Hide / show info and preview", true, false),
    ClientBrowserExit => ("client.browser.exit", "Exit browser", true, false),
    SessionRefresh => ("session.refresh", "Refresh browser", true, false),
    ClientCreate => ("client.create", "Create focused kind", true, false),
    ClientRename => ("client.rename", "Rename focused entry", true, false),
    ClientClose => ("client.close", "Close focused entry (confirm)", true, false),
    SessionNew => ("session.new", "Create session", true, false),
    SessionRename => ("session.rename", "Rename session", true, false),
    SessionClose => ("session.close", "Close session (confirm)", true, false),
    ClientTabNew => ("client.tab.new", "Create tab", true, true),
    ClientTabRename => ("client.tab.rename", "Rename tab", true, false),
    ClientTabClose => ("client.tab.close", "Close tab (confirm)", true, false),
    PaneSplit => ("pane.split", "Split side by side", true, true),
    PaneSplitRight => ("pane.split.right", "Split side by side", true, true),
    PaneSplitDown => ("pane.split.down", "Split above / below", true, true),
    PaneRename => ("pane.rename", "Rename block", true, false),
    PaneClose => ("pane.close", "Close block (confirm)", true, false),
    PaneFocusNext => ("pane.focus_next", "Attach next block", false, true),
    ClientTabNext => ("client.tab.next", "Attach next tab", false, true),
    ClientTabPrevious => ("client.tab.previous", "Attach previous tab", false, true),
    ClientDetach => ("client.detach", "Return to browser", false, true),
    ClientSendPrefix => ("client.send_prefix", "Send literal Ctrl-B", false, true),
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
impl PartialEq<str> for Action {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for Action {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
#[cfg(test)]
impl From<&str> for Action {
    fn from(name: &str) -> Self {
        name.parse().expect("test action")
    }
}
