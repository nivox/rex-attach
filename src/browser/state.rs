use super::{
    dialog::{Dialog, Operation},
    model::*,
};
use crate::{activity, domain::Selection, domain::Target, shortcuts::Bindings};
use anyhow::{Context, Result};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewLayout {
    Column,
    #[default]
    Row,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Focus {
    #[default]
    Sessions,
    Windows,
    Blocks,
}

#[derive(Default)]
pub struct Browser {
    pub(super) preview_layout: PreviewLayout,
    pub(super) preview_hidden: bool,
    pub(super) preview_options: crate::preview::Options,
    pub(super) bindings: Bindings,
    pub(super) terminal_bindings: Bindings,
    pub(super) dialog: Option<Dialog>,
    pub(super) help: Option<crate::help::Help>,
    pub(super) sessions: Vec<Session>,
    pub(super) session: usize,
    pub(super) window: usize,
    pub(super) block: usize,
    pub(super) focus: Focus,
}
impl Browser {
    pub(super) fn inspection(&self) -> activity::Request {
        activity::Request {
            targets: self
                .sessions
                .iter()
                .flat_map(|s| {
                    s.windows.iter().flat_map(move |t| {
                        t.blocks.iter().filter(|b| b.terminal).map(move |b| Target {
                            session: s.id.clone(),
                            block: b.id.clone(),
                        })
                    })
                })
                .collect(),
            selected: self.target(),
        }
    }

    pub fn new(
        preview_options: crate::preview::Options,
        preview_layout: PreviewLayout,
        bindings: Bindings,
        terminal_bindings: Bindings,
    ) -> Self {
        Self {
            preview_options,
            preview_layout,
            bindings,
            terminal_bindings,
            ..Default::default()
        }
    }
    pub(super) fn session(&self) -> Option<&Session> {
        self.sessions.get(self.session)
    }
    pub(super) fn window(&self) -> Option<&Window> {
        self.session()?.windows.get(self.window)
    }
    pub(super) fn block(&self) -> Option<&Block> {
        self.window()?.blocks.get(self.block)
    }
    pub(super) fn show_blocks(&self) -> bool {
        self.window().is_some_and(|t| t.blocks.len() != 1)
    }
    pub(super) fn target(&self) -> Option<Target> {
        let block = self.block()?;
        let session = self.session()?.id.clone();
        block.terminal.then(|| Target {
            session,
            block: block.id.clone(),
        })
    }
    pub(super) fn normalize(&mut self) {
        self.session = self.session.min(self.sessions.len().saturating_sub(1));
        self.window = self.window.min(
            self.session()
                .map_or(0, |s| s.windows.len().saturating_sub(1)),
        );
        self.block = self.block.min(
            self.window()
                .map_or(0, |t| t.blocks.len().saturating_sub(1)),
        );
        if self.focus == Focus::Blocks && !self.show_blocks() {
            self.focus = Focus::Windows;
        }
    }
    pub(super) fn replace(&mut self, sessions: Vec<Session>) {
        let session = self.session().map(|s| s.id.clone());
        let window = self.window().map(|t| t.id.clone());
        let block = self.block().map(|b| b.id.clone());
        self.sessions = sessions;
        self.session = self
            .sessions
            .iter()
            .position(|s| Some(&s.id) == session.as_ref())
            .unwrap_or(0);
        self.window = self
            .session()
            .and_then(|s| {
                s.windows
                    .iter()
                    .position(|t| Some(&t.id) == window.as_ref())
            })
            .unwrap_or(0);
        self.block = self
            .window()
            .and_then(|t| t.blocks.iter().position(|b| Some(&b.id) == block.as_ref()))
            .unwrap_or(0);
        self.normalize();
    }
    pub(super) fn restore(&mut self, target: &Target) {
        for (s, session) in self.sessions.iter().enumerate() {
            if session.id != target.session {
                continue;
            }
            for (t, window) in session.windows.iter().enumerate() {
                if let Some(b) = window.blocks.iter().position(|b| b.id == target.block) {
                    self.session = s;
                    self.window = t;
                    self.block = b;
                    self.focus = if window.blocks.len() == 1 {
                        Focus::Windows
                    } else {
                        Focus::Blocks
                    };
                    return;
                }
            }
        }
    }
    pub(super) fn vertical(&mut self, down: bool) {
        let (selected, len) = match self.focus {
            Focus::Sessions => (self.session, self.sessions.len()),
            Focus::Windows => (self.window, self.session().map_or(0, |s| s.windows.len())),
            Focus::Blocks => (self.block, self.window().map_or(0, |t| t.blocks.len())),
        };
        if len == 0 {
            return;
        }
        let next = if down {
            (selected + 1) % len
        } else {
            (selected + len - 1) % len
        };
        match self.focus {
            Focus::Sessions => {
                self.session = next;
                self.window = 0;
                self.block = 0;
            }
            Focus::Windows => {
                self.window = next;
                self.block = 0;
            }
            Focus::Blocks => self.block = next,
        }
        self.normalize();
    }
    pub(super) fn horizontal(&mut self, right: bool) {
        self.focus = match (self.focus, right) {
            (Focus::Sessions, true) if self.session().is_some() => Focus::Windows,
            (Focus::Windows, true) if self.window().is_some_and(|t| t.blocks.len() > 1) => {
                Focus::Blocks
            }
            (Focus::Blocks, false) => Focus::Windows,
            (Focus::Windows, false) => Focus::Sessions,
            _ => self.focus,
        };
    }
    pub(super) fn choose(&mut self) -> Option<Target> {
        match self.focus {
            Focus::Blocks => self.target(),
            Focus::Windows if self.window().is_some_and(|t| t.blocks.len() == 1) => self.target(),
            _ => {
                self.horizontal(true);
                None
            }
        }
    }
}

impl Browser {
    pub(super) fn selection(&self, focus: Focus) -> Result<(Selection, String)> {
        let session = self.session().context("No session selected")?;
        match focus {
            Focus::Sessions => Ok((Selection::Session(session.id.clone()), session.name.clone())),
            Focus::Windows => {
                let window = self.window().context("No tab selected")?;
                anyhow::ensure!(
                    window.id != "detached",
                    "Detached blocks are not a server window; select a block instead"
                );
                Ok((
                    Selection::Window {
                        session: session.id.clone(),
                        window: window.id.clone(),
                    },
                    window.name.clone(),
                ))
            }
            Focus::Blocks => {
                let block = self.block().context("No block selected")?;
                Ok((
                    Selection::Block(Target {
                        session: session.id.clone(),
                        block: block.id.clone(),
                    }),
                    block.name.clone(),
                ))
            }
        }
    }
    pub(super) fn start_dialog(&self, action: &str) -> Result<Dialog> {
        let (operation, focus, direction) = match action {
            "client.create" => (Operation::Create, self.focus, "horizontal"),
            "client.rename" => (Operation::Rename, self.focus, "horizontal"),
            "client.close" => (Operation::Close, self.focus, "horizontal"),
            "session.new" => (Operation::Create, Focus::Sessions, "horizontal"),
            "session.rename" => (Operation::Rename, Focus::Sessions, "horizontal"),
            "session.close" => (Operation::Close, Focus::Sessions, "horizontal"),
            "client.tab.new" => (Operation::Create, Focus::Windows, "horizontal"),
            "client.tab.rename" => (Operation::Rename, Focus::Windows, "horizontal"),
            "client.tab.close" => (Operation::Close, Focus::Windows, "horizontal"),
            "pane.split" | "pane.split.right" => (Operation::Create, Focus::Blocks, "horizontal"),
            "pane.split.down" => (Operation::Create, Focus::Blocks, "vertical"),
            "pane.rename" => (Operation::Rename, Focus::Blocks, "horizontal"),
            "pane.close" => (Operation::Close, Focus::Blocks, "horizontal"),
            _ => anyhow::bail!("Unsupported browser action: {action}"),
        };
        let (selection, name) = if operation == Operation::Create && focus == Focus::Sessions {
            (Selection::Session(String::new()), String::new())
        } else if operation == Operation::Create && focus == Focus::Windows {
            (
                Selection::Window {
                    session: self.session().context("No session selected")?.id.clone(),
                    window: String::new(),
                },
                String::new(),
            )
        } else {
            self.selection(focus)?
        };
        Ok(Dialog {
            text: if operation == Operation::Rename {
                name.clone()
            } else {
                String::new()
            },
            operation,
            selection,
            name,
            direction,
            pasting: false,
        })
    }
    pub(super) fn select_id(&mut self, id: &str) {
        for (si, session) in self.sessions.iter().enumerate() {
            if session.id == id {
                self.session = si;
                self.window = 0;
                self.block = 0;
                self.normalize();
                return;
            }
            for (wi, window) in session.windows.iter().enumerate() {
                if window.id == id {
                    self.session = si;
                    self.window = wi;
                    self.block = 0;
                    self.normalize();
                    return;
                }
                if let Some(bi) = window.blocks.iter().position(|b| b.id == id) {
                    self.session = si;
                    self.window = wi;
                    self.block = bi;
                    self.normalize();
                    return;
                }
            }
        }
    }
}

/// Pure UI transitions. The controller alone executes the returned effects.
pub(super) enum Effect {
    None,
    Exit,
    Attach(Target),
    Refresh,
    Mutate(Dialog),
}
impl Browser {
    pub(super) fn feed(&mut self, event: crate::shortcuts::Event) -> Result<Effect> {
        use crate::shortcuts::Event;
        if let Some(help) = &mut self.help {
            if help.feed(
                event,
                crate::help::row_count(&self.bindings, &self.terminal_bindings),
            ) {
                self.help = None;
            }
            return Ok(Effect::None);
        }
        if let Some(dialog) = &mut self.dialog {
            return match dialog.feed(&event.bytes()) {
                Some(commit) => {
                    let dialog = self.dialog.take().expect("active dialog");
                    Ok(if commit {
                        Effect::Mutate(dialog)
                    } else {
                        Effect::None
                    })
                }
                None => Ok(Effect::None),
            };
        }
        let Event::Action(action, _) = event else {
            return Ok(Effect::None);
        };
        match action.as_str() {
            "client.shortcuts" => self.help = Some(crate::help::Help::default()),
            "client.browser.exit" => return Ok(Effect::Exit),
            "client.browser.up" => self.vertical(false),
            "client.browser.down" => self.vertical(true),
            "client.browser.left" => self.horizontal(false),
            "client.browser.right" => self.horizontal(true),
            "client.browser.toggle_preview" => self.preview_hidden = !self.preview_hidden,
            "client.browser.choose" => {
                if let Some(target) = self.choose() {
                    return Ok(Effect::Attach(target));
                }
                anyhow::ensure!(
                    !self.block().is_some_and(|b| !b.terminal),
                    "Select a terminal block to attach."
                );
            }
            "session.refresh" => return Ok(Effect::Refresh),
            action => self.dialog = Some(self.start_dialog(action)?),
        }
        Ok(Effect::None)
    }
}
