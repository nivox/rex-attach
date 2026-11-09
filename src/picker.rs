use crate::{
    Target,
    actions::{self, Selection},
    activity::{self, Inspector, Snapshot, State},
    preview::Preview,
    protocol::Control,
    shortcuts::{Bindings, Event, Matcher},
    tty,
};
use anyhow::{Context, Result};
use ratatui::{
    Frame as UiFrame, Terminal as UiTerminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as UiBlock, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
struct Block {
    id: String,
    name: String,
    creator: String,
    flavor: String,
    terminal: bool,
    default_label: bool,
}
#[derive(Clone, Debug)]
// Server windows are presented as tabs in Rex clients.
struct Window {
    id: String,
    name: String,
    blocks: Vec<Block>,
    default_label: bool,
    focused_block: Option<String>,
}
#[derive(Clone, Debug)]
struct Session {
    id: String,
    name: String,
    windows: Vec<Window>,
}

fn values(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}
fn short(id: &str) -> &str {
    id.get(id.len().saturating_sub(6)..).unwrap_or(id)
}
fn name(value: &Value, fallback: &str) -> String {
    value["label"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}
// Rex exposes no label provenance. Only match the generated label for this ID.
fn default_label(value: &Value, kind: &str, id: &str) -> bool {
    let label = value["label"].as_str().unwrap_or("");
    label.is_empty()
        || label
            .strip_prefix(&format!("{kind} "))
            .is_some_and(|suffix| suffix.len() == 8 && id.ends_with(suffix))
}
impl Block {
    fn recognize_flavor_default(&mut self, creators: &Value) {
        self.default_label |= values(&creators["creators"])
            .iter()
            .flat_map(|creator| values(&creator["flavors"]))
            .any(|flavor| {
                flavor["flavor"] == self.flavor
                    && (flavor["name"] == self.name || flavor["title"] == self.name)
            });
    }
    fn display_name<'a>(&'a self, snapshot: &'a Snapshot, session: &str) -> &'a str {
        if self.default_label {
            let target = Target {
                session: session.into(),
                block: self.id.clone(),
            };
            if let Some(title) = snapshot
                .info(&target)
                .and_then(|info| info.title.as_deref())
                .filter(|title| !clean(title).trim().is_empty())
            {
                return title;
            }
        }
        &self.name
    }
}
impl Window {
    fn display_name<'a>(&'a self, snapshot: &'a Snapshot, session: &str) -> &'a str {
        if self.default_label {
            let focused = self
                .focused_block
                .as_deref()
                .and_then(|id| self.blocks.iter().find(|block| block.id == id))
                .or_else(|| self.blocks.first());
            if let Some(title) = focused
                .and_then(|block| {
                    snapshot.info(&Target {
                        session: session.into(),
                        block: block.id.clone(),
                    })
                })
                .and_then(|info| info.title.as_deref())
                .filter(|title| !clean(title).trim().is_empty())
            {
                return title;
            }
        }
        &self.name
    }
}
fn block(value: &Value) -> Result<Block> {
    let id = value["block_id"]
        .as_str()
        .context("block has no ID")?
        .to_owned();
    let flavor = value["flavor"].as_str().unwrap_or("unknown flavor");
    let terminal = value["creator_name"] == "com.superlogical.terminal"
        || flavor.starts_with("com.superlogical.terminal.");
    Ok(Block {
        default_label: default_label(value, "Block", &id),
        name: name(value, &format!("Block {}", short(&id))),
        id,
        creator: value["creator_name"].as_str().unwrap_or("unknown").into(),
        flavor: flavor.into(),
        terminal,
    })
}

fn session_tree(session: &Value, view: &Value, blocks: &[Value]) -> Result<Session> {
    let id = session["session_id"]
        .as_str()
        .context("session has no ID")?
        .to_owned();
    let mut windows = Vec::new();
    for (index, window) in values(&view["windows"]).iter().enumerate() {
        let mut children = Vec::new();
        for layer in values(&window["layers"]) {
            for child in values(&layer["blocks"]) {
                let child_id = child["block_id"]
                    .as_str()
                    .context("window block has no ID")?;
                if children.iter().any(|b: &Block| b.id == child_id) {
                    continue;
                }
                // list_blocks carries placement and ownership metadata; view
                // defines window/layer order and also covers concurrent additions.
                let metadata = blocks
                    .iter()
                    .find(|b| b["block_id"] == child_id)
                    .unwrap_or(child);
                children.push(block(metadata)?);
            }
        }
        windows.push(Window {
            default_label: default_label(
                window,
                "Window",
                window["window_id"].as_str().unwrap_or(""),
            ),
            focused_block: window["focused_block_id"].as_str().map(str::to_owned),
            id: window["window_id"]
                .as_str()
                .context("window has no ID")?
                .into(),
            name: name(window, &format!("Tab {}", index + 1)),
            blocks: children,
        });
    }
    let detached = blocks
        .iter()
        .filter(|b| b["placement"] == "detached")
        .map(block)
        .collect::<Result<Vec<_>>>()?;
    if !detached.is_empty() {
        windows.push(Window {
            default_label: false,
            focused_block: None,
            id: "detached".into(),
            name: "Detached blocks".into(),
            blocks: detached,
        });
    }
    Ok(Session {
        name: name(session, &id),
        id,
        windows,
    })
}

async fn load(control: &mut Control, scope: Option<&str>) -> Result<Vec<Session>> {
    // Older servers may not expose creator metadata; ID defaults still work.
    let creators = control
        .call("block_creator.list", json!({}))
        .await
        .unwrap_or(Value::Null);
    let reply = control.call("session.list", json!({})).await?;
    let mut sessions = Vec::new();
    for session in values(&reply["sessions"]) {
        let id = session["session_id"]
            .as_str()
            .context("session has no ID")?;
        if scope.is_some_and(|scope| scope != id) {
            continue;
        }
        let attached = control
            .call("session.attach", json!({"session_id":id}))
            .await?;
        let blocks = control
            .call("session.list_blocks", json!({"session_id":id}))
            .await;
        let detached = control
            .call("session.detach", json!({"session_id":id}))
            .await;
        let blocks = blocks?;
        detached?;
        let mut tree = session_tree(session, &attached["view"], values(&blocks["blocks"]))?;
        for block in tree
            .windows
            .iter_mut()
            .flat_map(|window| &mut window.blocks)
        {
            block.recognize_flavor_default(&creators);
        }
        sessions.push(tree);
    }
    Ok(sessions)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Focus {
    #[default]
    Sessions,
    Windows,
    Blocks,
}

#[derive(Default)]
pub struct Browser {
    preview_options: crate::preview::Options,
    bindings: Bindings,
    terminal_bindings: Bindings,
    dialog: Option<Dialog>,
    help: Option<crate::help::Help>,
    sessions: Vec<Session>,
    session: usize,
    window: usize,
    block: usize,
    focus: Focus,
}
impl Browser {
    fn inspection(&self) -> activity::Request {
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
        bindings: Bindings,
        terminal_bindings: Bindings,
    ) -> Self {
        Self {
            preview_options,
            bindings,
            terminal_bindings,
            ..Default::default()
        }
    }
    fn session(&self) -> Option<&Session> {
        self.sessions.get(self.session)
    }
    fn window(&self) -> Option<&Window> {
        self.session()?.windows.get(self.window)
    }
    fn block(&self) -> Option<&Block> {
        self.window()?.blocks.get(self.block)
    }
    fn show_blocks(&self) -> bool {
        self.window().is_some_and(|t| t.blocks.len() != 1)
    }
    fn target(&self) -> Option<Target> {
        let block = self.block()?;
        let session = self.session()?.id.clone();
        block.terminal.then(|| Target {
            session,
            block: block.id.clone(),
        })
    }
    fn normalize(&mut self) {
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
    fn replace(&mut self, sessions: Vec<Session>) {
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
    fn restore(&mut self, target: &Target) {
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
    fn vertical(&mut self, down: bool) {
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
    fn horizontal(&mut self, right: bool) {
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
    fn choose(&mut self) -> Option<Target> {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Create,
    Rename,
    Close,
}
struct Dialog {
    operation: Operation,
    selection: Selection,
    name: String,
    text: String,
    direction: &'static str,
    pasting: bool,
}
impl Browser {
    fn selection(&self, focus: Focus) -> Result<(Selection, String)> {
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
    fn start_dialog(&self, action: &str) -> Result<Dialog> {
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
    fn select_id(&mut self, id: &str) {
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
impl Dialog {
    /// Some(true) commits; Some(false) cancels. Close defaults to cancellation.
    fn feed(&mut self, bytes: &[u8]) -> Option<bool> {
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
    async fn execute(self, control: &mut Control) -> Result<Option<String>> {
        match self.operation {
            Operation::Create => Ok(Some(
                actions::create(control, &self.selection, &self.text, self.direction).await?,
            )),
            Operation::Rename => {
                actions::rename(control, &self.selection, &self.text).await?;
                Ok(None)
            }
            Operation::Close => {
                actions::close(control, &self.selection).await?;
                Ok(None)
            }
        }
    }
    fn render(&self, frame: &mut UiFrame) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(76);
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + 3.min(area.height.saturating_sub(1)),
            width,
            7.min(area.height.saturating_sub(3)),
        );
        let kind = self.selection.kind();
        let (title, body) = match self.operation {
            Operation::Close => (
                format!("Close {kind}?"),
                format!(
                    "{}\nThis closes its programs and all contents.\n\nY: close   N/Enter/Esc: cancel",
                    clean(&self.name)
                ),
            ),
            Operation::Create => (
                format!("Create {kind}"),
                format!(
                    "Label: {}▏\n\nEnter: create (blank: default label)   Esc: cancel",
                    clean(&self.text)
                ),
            ),
            Operation::Rename => (
                format!("Rename {kind}"),
                format!(
                    "Label: {}▏\n\nEnter: rename   Ctrl-U: clear   Esc: cancel",
                    clean(&self.text)
                ),
            ),
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: false }).block(
                UiBlock::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow)),
            ),
            rect,
        );
    }
}

struct Screen {
    terminal: UiTerminal<CrosstermBackend<io::Stdout>>,
    image: Option<Arc<crate::preview::RenderedImage>>,
}
impl Screen {
    fn new() -> Result<Self> {
        // Our outer Guard owns raw mode and the alternate screen. Ratatui owns
        // only browser drawing, so attaching never nests terminal setup.
        let mut terminal = UiTerminal::new(CrosstermBackend::new(io::stdout()))?;
        // Fullscreen setup needs no cursor-position query. Avoid clear(),
        // which asks the backend to read stdin and would compete with Input.
        terminal.backend_mut().write_all(b"\x1b[2J\x1b[H")?;
        Ok(Self {
            terminal,
            image: None,
        })
    }
    fn draw(
        &mut self,
        browser: &Browser,
        preview: &crate::preview::Frame,
        snapshot: &Snapshot,
        notice: &str,
    ) -> Result<()> {
        if self.image.as_ref().map(|i| i.id) != preview.image.as_ref().map(|i| i.id)
            && let Some(image) = self.image.take()
        {
            image.delete(self.terminal.backend_mut())?;
        }
        self.image = preview.image.clone();
        self.terminal.backend_mut().write_all(b"\x1b[?2026h")?;
        let result = self
            .terminal
            .draw(|frame| render(frame, browser, preview, snapshot, notice))
            .map(|_| ());
        self.terminal.backend_mut().write_all(b"\x1b[?2026l")?;
        self.terminal.backend_mut().flush()?;
        result?;
        Ok(())
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        if let Some(image) = self.image.take() {
            let _ = image.delete(self.terminal.backend_mut());
            let _ = self.terminal.backend_mut().flush();
        }
    }
}

fn pane_rects(browser: &Browser, area: Rect) -> Vec<Rect> {
    let sides = if browser.show_blocks() { 3 } else { 2 };
    let side = (area.width / (sides + 1)).max(1);
    let mut constraints = vec![Constraint::Length(side); sides as usize];
    constraints.push(Constraint::Min(1));
    let body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(2),
    );
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(body)
        .to_vec()
}
fn preview_size(browser: &Browser, size: (u16, u16)) -> (u16, u16) {
    if size.0 < 40 || size.1 < 8 {
        return (0, 0);
    }
    let panes = pane_rects(browser, Rect::new(0, 0, size.0, size.1));
    let area = UiBlock::default()
        .borders(Borders::ALL)
        .inner(right_rects(*panes.last().unwrap())[1]);
    (area.width, area.height)
}
fn right_rects(pane: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(pane)
}
fn block_state(snapshot: &Snapshot, session: &str, block: &Block) -> State {
    snapshot.state(&Target {
        session: session.into(),
        block: block.id.clone(),
    })
}
fn window_state(snapshot: &Snapshot, session: &str, window: &Window) -> State {
    activity::aggregate(
        window
            .blocks
            .iter()
            .map(|b| block_state(snapshot, session, b)),
    )
}
fn session_state(snapshot: &Snapshot, session: &Session) -> State {
    activity::aggregate(
        session
            .windows
            .iter()
            .map(|t| window_state(snapshot, &session.id, t)),
    )
}
fn entry(state: State, label: String) -> ListItem<'static> {
    if state == State::Unknown {
        return ListItem::new(label);
    }
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{} ", state.badge()),
            Style::default().fg(state.color()),
        ),
        Span::raw(label),
    ]))
}
fn information(frame: &mut UiFrame, browser: &Browser, snapshot: &Snapshot, area: Rect) {
    let border = UiBlock::default()
        .borders(Borders::ALL)
        .title("Block information")
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = border.inner(area);
    frame.render_widget(border, area);
    let Some(block) = browser.block() else {
        frame.render_widget(Paragraph::new("No block selected"), inner);
        return;
    };
    let target = browser.session().map(|s| Target {
        session: s.id.clone(),
        block: block.id.clone(),
    });
    let info = target.as_ref().and_then(|t| snapshot.info(t));
    let pending =
        block.terminal && snapshot.error.is_none() && info.is_none_or(|info| !info.loaded);
    let missing = if pending { "Loading…" } else { "unavailable" };
    let fields = [
        format!("Label: {}", clean(&block.name)),
        format!("Type: {}", clean(&block.creator)),
        format!("Flavor: {}", clean(&block.flavor)),
        format!(
            "Title: {}",
            info.and_then(|i| i.title.as_deref())
                .map(clean)
                .unwrap_or_else(|| missing.into())
        ),
        format!(
            "Directory: {}",
            info.and_then(|i| i.cwd.as_deref())
                .map(clean)
                .unwrap_or_else(|| missing.into())
        ),
        format!(
            "Process: {}",
            info.and_then(|i| i.foreground.as_deref())
                .map(clean)
                .unwrap_or_else(|| missing.into())
        ),
    ];
    let state = info.map_or(State::Unknown, activity::Info::state);
    let [fields_area, status_area, messages_area] = Layout::vertical([
        Constraint::Length(6),
        Constraint::Length(u16::from(state != State::Unknown)),
        Constraint::Min(0),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(fields.join("\n")), fields_area);
    let summary = info
        .and_then(activity::Info::primary)
        .map(activity::Record::summary)
        .unwrap_or_default();
    if state != State::Unknown {
        frame.render_widget(
            Paragraph::new(format!("Status: {} {}", state.badge(), clean(&summary)))
                .style(Style::default().fg(state.color())),
            status_area,
        );
    }
    let mut messages = info.map(activity::Info::messages).unwrap_or_default();
    if let Some(error) = info
        .and_then(|i| i.status_error.as_ref().or(i.details_error.as_ref()))
        .or(snapshot.error.as_ref())
    {
        messages.push(error.clone());
    }
    frame.render_widget(
        Paragraph::new(
            messages
                .iter()
                .map(|s| clean(s))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .wrap(Wrap { trim: false }),
        messages_area,
    );
}
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}
fn render(
    frame: &mut UiFrame,
    browser: &Browser,
    preview: &crate::preview::Frame,
    snapshot: &Snapshot,
    notice: &str,
) {
    if let Some(help) = &browser.help {
        help.render(frame, &browser.bindings, &browser.terminal_bindings);
        return;
    }
    let area = frame.area();
    if area.is_empty() {
        return;
    }
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(
                "Arrows/hjkl: navigate | Enter: attach | c: create | r: rename | x: close | ?: help | q/Esc: exit",
            ),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    if area.width < 40 {
        frame.render_widget(
            Paragraph::new("Widen terminal to browse (40+ cols). Esc: exit"),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
        return;
    }
    let panes = pane_rects(browser, area);
    let mut columns = vec![
        (
            "Sessions",
            Focus::Sessions,
            browser.session,
            browser
                .sessions
                .iter()
                .map(|s| entry(session_state(snapshot, s), clean(&s.name)))
                .collect::<Vec<_>>(),
        ),
        (
            "Tabs",
            Focus::Windows,
            browser.window,
            browser
                .session()
                .map(|s| {
                    s.windows
                        .iter()
                        .map(|t| {
                            entry(
                                window_state(snapshot, &s.id, t),
                                format!(
                                    "{} ({})",
                                    clean(t.display_name(snapshot, &s.id)),
                                    t.blocks.len()
                                ),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ),
    ];
    if browser.show_blocks() {
        columns.push((
            "Blocks",
            Focus::Blocks,
            browser.block,
            browser
                .window()
                .map(|t| {
                    t.blocks
                        .iter()
                        .map(|b| {
                            entry(
                                block_state(snapshot, &browser.session().unwrap().id, b),
                                clean(b.display_name(snapshot, &browser.session().unwrap().id)),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
        ));
    }
    for (index, (title, focus, selected, items)) in columns.into_iter().enumerate() {
        let active = browser.focus == focus;
        let border = UiBlock::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }));
        let items = if items.is_empty() {
            vec![ListItem::new("(empty)")]
        } else {
            items
        };
        let list = List::new(items)
            .block(border)
            .highlight_symbol("> ")
            .highlight_style(if active {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            });
        let mut selected = ListState::default().with_selected(Some(selected));
        frame.render_stateful_widget(list, panes[index], &mut selected);
    }
    let [info_area, pane] = right_rects(*panes.last().unwrap());
    information(frame, browser, snapshot, info_area);
    let title = if browser.preview_options.images() {
        "Preview (Kitty)"
    } else {
        "Preview (text)"
    };
    let border = UiBlock::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = border.inner(pane);
    frame.render_widget(border, pane);
    if let Some(image) = &preview.image {
        frame.render_widget(
            ratatui_image::Image::new(&image.protocol).allow_clipping(true),
            inner,
        );
    } else {
        let text = preview
            .text
            .lines()
            .map(clean)
            .collect::<Vec<_>>()
            .join("\n");
        frame.render_widget(Paragraph::new(text), inner);
    }
    frame.render_widget(
        Paragraph::new(if notice.is_empty() {
            format!(
                "Detach from a block with {} | Help when attached with {}",
                browser.terminal_bindings.shortcut("client.detach"),
                browser.terminal_bindings.shortcut("client.shortcuts")
            )
        } else {
            clean(notice)
        }),
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
    );
    if let Some(dialog) = &browser.dialog {
        dialog.render(frame);
    }
}

pub async fn pick(
    socket: &str,
    control: &mut Control,
    scope: Option<&str>,
    input: &mut tty::Input,
    notice: &str,
    browser: &mut Browser,
    previous: Option<&Target>,
) -> Result<Option<Target>> {
    let initial = browser.sessions.is_empty();
    browser.replace(load(control, scope).await?);
    if initial && scope.is_some() {
        browser.focus = Focus::Windows;
    }
    if let Some(previous) = previous
        && (initial || browser.target().as_ref() != Some(previous))
    {
        browser.restore(previous);
    }
    let dimensions = tty::size()?;
    let target = browser.target();
    let size = preview_size(browser, dimensions);
    let mut pane = Pane {
        worker: Preview::start(
            socket,
            target.clone(),
            size.0,
            size.1,
            browser.preview_options.clone(),
        ),
        target,
        size,
        open: true,
        inspector: Inspector::start(socket, browser.inspection()),
    };
    let result = pick_inner(socket, control, scope, input, notice, browser, &mut pane).await;
    pane.worker.stop().await;
    pane.inspector.stop().await;
    result
}

struct Pane {
    inspector: Inspector,
    worker: Preview,
    target: Option<Target>,
    size: (u16, u16),
    open: bool,
}

async fn pick_inner(
    socket: &str,
    control: &mut Control,
    scope: Option<&str>,
    input: &mut tty::Input,
    notice: &str,
    browser: &mut Browser,
    pane: &mut Pane,
) -> Result<Option<Target>> {
    let mut dimensions = tty::size()?;
    let mut notice = notice.to_owned();
    let mut keys = Matcher::new(&browser.bindings);
    let mut bytes = [0u8; 256];
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.tick().await;
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut escape_deadline = None;
    let mut screen = Screen::new()?;
    let mut inspector_open = true;
    loop {
        let target = browser.target();
        let size = preview_size(browser, dimensions);
        if pane.target != target || pane.size != size {
            let old = std::mem::replace(
                &mut pane.worker,
                Preview::start(
                    socket,
                    target.clone(),
                    size.0,
                    size.1,
                    browser.preview_options.clone(),
                ),
            );
            old.stop().await;
            pane.target = target;
            pane.size = size;
            pane.open = true;
        }
        pane.inspector.update(browser.inspection());
        screen.draw(
            browser,
            &pane.worker.frame.borrow(),
            &pane.inspector.snapshot.borrow(),
            &notice,
        )?;
        let actions = tokio::select! {
            result=input.read(&mut bytes)=> {let n=result?;if n==0 {return Ok(None);}let actions=keys.feed(&bytes[..n]);escape_deadline=keys.waiting().then(||tokio::time::Instant::now()+keys.delay());actions},
            _=tokio::time::sleep_until(escape_deadline.unwrap_or_else(||tokio::time::Instant::now()+Duration::from_secs(3600))),if escape_deadline.is_some()=> {escape_deadline=None;keys.timeout()},
            result=pane.worker.frame.changed(),if pane.open=> {if result.is_err(){pane.open=false;}vec![]},
            result=pane.inspector.snapshot.changed(),if inspector_open=> {if result.is_err(){inspector_open=false;}vec![]},
            _=tick.tick()=>vec![Event::Action("session.refresh".into(),vec![])],
            _=winch.recv()=> {dimensions=tty::size()?;vec![]},
            _=interrupt.recv()=>return Ok(None),_=terminate.recv()=>return Ok(None),
            value=crate::protocol::next_json(&mut control.socket)=> {value?;vec![]},
        };
        let mut events = actions.into_iter();
        while let Some(event) = events.next() {
            if let Some(help) = &mut browser.help {
                if help.feed(
                    event,
                    crate::help::row_count(&browser.bindings, &browser.terminal_bindings),
                ) {
                    browser.help = None;
                }
                continue;
            }
            if browser.dialog.is_some() {
                if let Some(result) = browser.dialog.as_mut().unwrap().feed(&event.bytes()) {
                    let dialog = browser.dialog.take().unwrap();
                    if result {
                        match dialog.execute(control).await {
                            Ok(created) => match load(control, scope).await {
                                Ok(next) => {
                                    browser.replace(next);
                                    if let Some(id) = created {
                                        browser.select_id(&id);
                                        if id.starts_with("session:")
                                            && scope.is_some_and(|s| s != id)
                                        {
                                            notice = format!(
                                                "Created {id}. This browser remains scoped to its original session."
                                            );
                                        }
                                    }
                                }
                                Err(error) => notice = format!("Refresh failed: {error:#}"),
                            },
                            Err(error) => notice = format!("Action failed: {error:#}"),
                        }
                    }
                }
                continue;
            }
            let Event::Action(action, _) = event else {
                continue;
            };
            match action.as_str() {
                "client.shortcuts" => browser.help = Some(crate::help::Help::default()),
                "client.browser.exit" => return Ok(None),
                "client.browser.up" => browser.vertical(false),
                "client.browser.down" => browser.vertical(true),
                "client.browser.left" => browser.horizontal(false),
                "client.browser.right" => browser.horizontal(true),
                "client.browser.choose" => {
                    if let Some(target) = browser.choose() {
                        let mut rest: Vec<u8> = events.flat_map(Event::bytes).collect();
                        rest.extend(keys.remaining());
                        input.unread(&rest);
                        return Ok(Some(target));
                    }
                    if browser.block().is_some_and(|b| !b.terminal) {
                        notice = "Select a terminal block to attach.".into();
                    }
                }
                "session.refresh" => match load(control, scope).await {
                    Ok(next) => browser.replace(next),
                    Err(error) => notice = format!("Refresh failed: {error:#}"),
                },
                action => match browser.start_dialog(action) {
                    Ok(dialog) => {
                        notice.clear();
                        browser.dialog = Some(dialog);
                    }
                    Err(error) => notice = format!("{error:#}"),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Session {
        let b = |id: &str| json!({"block_id":id,"label":id,"creator_name":"com.superlogical.terminal","flavor":"com.superlogical.terminal.shell","placement":"placed"});
        session_tree(&json!({"session_id":"session:one","label":"work"}),&json!({"windows":[
            {"window_id":"window:one","label":"single","layers":[{"blocks":[b("block:a")]}]},
            {"window_id":"window:two","label":"split","layers":[{"blocks":[b("block:b")]},{"kind":"floating","blocks":[b("block:c")]}]}
        ]}),&[b("block:a"),b("block:b"),b("block:c"),json!({"block_id":"block:d","label":"loose","creator_name":"other","placement":"detached"})]).unwrap()
    }
    #[test]
    fn default_names_follow_titles_and_custom_names_stay() {
        let id = "block:01234567-89abcdef";
        let mut snapshot = Snapshot::default();
        let target = Target {
            session: "session:one".into(),
            block: id.into(),
        };
        snapshot.blocks.entry(target.clone()).or_default().title = Some("Build terminal".into());
        let creators = json!({"creators":[{"flavors":[{"name":"shell","flavor":"com.superlogical.terminal.shell","title":"Shell"}]}]});
        for (label, flavor, expected) in [
            ("shell", "com.superlogical.terminal.shell", "Build terminal"),
            ("Shell", "com.superlogical.terminal.shell", "Build terminal"),
            ("Shell", "other.flavor", "Shell"),
            ("My shell", "com.superlogical.terminal.shell", "My shell"),
        ] {
            let mut b = block(&json!({"block_id":id,"label":label,"flavor":flavor})).unwrap();
            b.recognize_flavor_default(&creators);
            assert_eq!(b.display_name(&snapshot, "session:one"), expected);
        }
        for (label, expected) in [
            ("Block 89abcdef", "Build terminal"),
            ("", "Build terminal"),
            ("Build", "Build"),
            ("Block 01234567", "Block 01234567"),
        ] {
            let b = block(&json!({"block_id":id,"label":label})).unwrap();
            assert_eq!(b.display_name(&snapshot, "session:one"), expected);
        }
        let mut session = session_tree(&json!({"session_id":"session:one"}), &json!({"windows":[{
            "window_id":"window:01234567-89abcdef", "label":"Window 89abcdef", "focused_block_id":id,
            "layers":[{"blocks":[{"block_id":"block:other","label":"Other"},{"block_id":id,"label":"Custom block"}]}]
        }]}), &[]).unwrap();
        let window = &mut session.windows[0];
        assert_eq!(
            window.display_name(&snapshot, "session:one"),
            "Build terminal"
        );
        snapshot.blocks.get_mut(&target).unwrap().title = Some("\u{1b}\n".into());
        assert_eq!(
            window.display_name(&snapshot, "session:one"),
            "Window 89abcdef"
        );
        window.default_label = false;
        window.name = "My tab".into();
        snapshot.blocks.get_mut(&target).unwrap().title = Some("Changed title".into());
        assert_eq!(window.display_name(&snapshot, "session:one"), "My tab");
    }
    #[test]
    fn tree_groups_tabs_layers_and_detached_blocks() {
        let session = fixture();
        assert_eq!(session.windows.len(), 3);
        assert_eq!(session.windows[0].name, "single");
        assert_eq!(
            session.windows[1]
                .blocks
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            vec!["block:b", "block:c"]
        );
        assert_eq!(session.windows[2].name, "Detached blocks");
        let browser = Browser {
            sessions: vec![session],
            window: 2,
            ..Default::default()
        };
        assert!(browser.target().is_none());
    }
    #[test]
    fn navigation_enters_columns_and_skips_single_block_column() {
        let mut browser = Browser {
            sessions: vec![fixture()],
            ..Default::default()
        };
        assert!(browser.choose().is_none());
        assert_eq!(browser.focus, Focus::Windows);
        assert!(!browser.show_blocks());
        browser.horizontal(true);
        assert_eq!(browser.focus, Focus::Windows);
        assert_eq!(browser.choose().unwrap().block, "block:a");
        browser.vertical(true);
        assert!(browser.show_blocks());
        assert!(browser.choose().is_none());
        assert_eq!(browser.focus, Focus::Blocks);
        browser.vertical(true);
        assert_eq!(browser.choose().unwrap().block, "block:c");
        browser.horizontal(false);
        assert_eq!(browser.focus, Focus::Windows);
        browser.horizontal(false);
        assert_eq!(browser.focus, Focus::Sessions);
    }
    #[test]
    fn refresh_and_detach_restore_by_ids_instead_of_positions() {
        let mut browser = Browser {
            sessions: vec![fixture()],
            ..Default::default()
        };
        let target = Target {
            session: "session:one".into(),
            block: "block:c".into(),
        };
        browser.restore(&target);
        assert_eq!(browser.focus, Focus::Blocks);
        let mut changed = fixture();
        changed.windows.swap(0, 1);
        changed.windows[0].blocks.reverse();
        browser.replace(vec![changed]);
        assert_eq!(browser.target(), Some(target));
        assert_eq!(browser.window, 0);
        assert_eq!(browser.block, 0);
        browser.restore(&Target {
            session: "session:one".into(),
            block: "block:a".into(),
        });
        assert_eq!(browser.focus, Focus::Windows);
    }

    fn test_screen(browser: &Browser, size: (u16, u16), text: &str) -> String {
        let mut terminal =
            UiTerminal::new(ratatui::backend::TestBackend::new(size.0, size.1)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    browser,
                    &crate::preview::Frame {
                        text: text.into(),
                        image: None,
                    },
                    &Snapshot::default(),
                    "",
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..size.1)
            .map(|y| {
                (0..size.0)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn dialogs_capture_identity_cancel_by_default_and_edit_unicode() {
        let mut browser = Browser {
            sessions: vec![fixture()],
            ..Default::default()
        };
        let mut dialog = browser.start_dialog("client.close").unwrap();
        assert!(matches!(&dialog.selection,Selection::Session(id) if id=="session:one"));
        assert_eq!(dialog.feed(b"\r"), Some(false));
        dialog.feed(b"\x1b[200~");
        assert_eq!(dialog.feed(b"y"), None);
        dialog.feed(b"\x1b[201~");
        assert_eq!(dialog.feed(b"y"), Some(true));
        browser.focus = Focus::Windows;
        let mut dialog = browser.start_dialog("client.rename").unwrap();
        assert_eq!(dialog.text, "single");
        dialog.feed(b"\x15");
        dialog.feed("界".as_bytes());
        dialog.feed(b"x");
        dialog.feed(b"\x7f");
        assert_eq!(dialog.text, "界");
        assert_eq!(dialog.feed(b"\x1b"), Some(false));
        browser.window = 1;
        browser.focus = Focus::Blocks;
        browser.block = 1;
        let dialog = browser.start_dialog("client.create").unwrap();
        assert!(matches!(dialog.selection,Selection::Block(t) if t.block=="block:c"));
        browser.window = 2;
        browser.focus = Focus::Windows;
        assert!(browser.start_dialog("client.close").is_err());
        assert!(browser.start_dialog("client.tab.new").is_ok());
        browser.dialog = Some(browser.start_dialog("session.new").unwrap());
        for size in [(120, 40), (20, 4), (1, 1)] {
            test_screen(&browser, size, "");
        }
    }

    #[test]
    fn layout_skips_blocks_for_single_tab_and_sanitizes_preview() {
        let mut browser = Browser {
            sessions: vec![fixture()],
            ..Default::default()
        };
        let screen = test_screen(&browser, (120, 24), "hello\x1b\n界");
        assert!(screen.contains("Preview (text)"));
        assert!(!screen.contains("Blocks"));
        assert!(screen.contains("hello"));
        assert!(!screen.contains('\x1b'));
        browser.window = 1;
        assert!(test_screen(&browser, (120, 24), "").contains("Blocks"));
        for width in [1, 20, 40, 80] {
            for height in [1, 3, 8, 24] {
                test_screen(&browser, (width, height), "");
            }
        }
        assert_eq!(clean("a\x1b\n界z"), "a界z");
    }
    #[test]
    fn compact_browser_uses_configured_footer_and_equal_column_widths() {
        let mut browser = Browser {
            sessions: vec![fixture()],
            window: 1,
            ..Default::default()
        };
        browser.terminal_bindings.display = vec![
            ("ctrl+a>d".into(), "client.detach".into()),
            ("ctrl+a>?".into(), "client.shortcuts".into()),
        ];
        let screen = test_screen(&browser, (120, 40), "");
        assert!(!screen.contains("rex-attach |"));
        assert!(!screen.contains("Column browser"));
        assert!(
            screen.lines().last().unwrap().starts_with(
                "Detach from a block with Ctrl-A d | Help when attached with Ctrl-A ?"
            )
        );
        assert_eq!(
            screen
                .lines()
                .filter(|line| line.contains("Label:"))
                .count(),
            1
        );
        let panes = pane_rects(&browser, Rect::new(0, 0, 120, 40));
        assert!(
            panes
                .iter()
                .all(|pane| pane.width == 30 && pane.y == 1 && pane.height == 38)
        );
    }
    #[test]
    fn unreported_activity_has_no_badge_legend_or_details_status_row() {
        let browser = Browser {
            sessions: vec![fixture()],
            ..Default::default()
        };
        let screen = test_screen(&browser, (120, 40), "terminal output");
        assert!(!screen.contains("unknown"));
        assert!(!screen.contains("Status:"));
        assert!(!screen.contains("[?]"));
        assert_eq!(State::Unknown.badge(), "");
        for state in [
            State::Idle,
            State::Done,
            State::Working,
            State::Error,
            State::Blocked,
        ] {
            assert!(!state.badge().is_ascii());
        }
    }
    #[test]
    fn information_and_preview_split_evenly_and_status_reaches_all_columns() {
        let mut browser = Browser {
            sessions: vec![fixture()],
            ..Default::default()
        };
        let target = Target {
            session: "session:one".into(),
            block: "block:c".into(),
        };
        browser.restore(&target);
        let mut snapshot = Snapshot::default();
        let mut info = activity::Info::default();
        info.title = Some("Build terminal".into());
        info.cwd = Some("/work/project".into());
        info.foreground = Some("cargo (PID 42)".into());
        info.loaded = true;
        info.records.insert(
            "".into(),
            activity::Record {
                state: State::Blocked,
                kind: Some("permission".into()),
                app: Some("cargo".into()),
                title: None,
                msg: Some("Approve deployment".into()),
                progress: Some(42),
            },
        );
        snapshot.blocks.insert(target, info);
        assert_eq!(
            block_state(&snapshot, "session:one", browser.block().unwrap()),
            State::Blocked
        );
        assert_eq!(
            window_state(&snapshot, "session:one", browser.window().unwrap()),
            State::Blocked
        );
        assert_eq!(
            session_state(&snapshot, browser.session().unwrap()),
            State::Blocked
        );
        let panes = pane_rects(&browser, Rect::new(0, 0, 120, 40));
        let [upper, lower] = right_rects(*panes.last().unwrap());
        assert!((i32::from(upper.height) - i32::from(lower.height)).abs() <= 1);
        assert_eq!(upper.bottom(), lower.y);
        let inner = UiBlock::default().borders(Borders::ALL).inner(lower);
        assert_eq!(
            preview_size(&browser, (120, 40)),
            (inner.width, inner.height)
        );
        let mut terminal = UiTerminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &browser,
                    &crate::preview::Frame {
                        text: "terminal output".into(),
                        image: None,
                    },
                    &snapshot,
                    "",
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = |y: u16, x: u16, width: u16| {
            (x..x + width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        };
        let upper_text = (upper.y..upper.bottom())
            .map(|y| row(y, upper.x, upper.width))
            .collect::<Vec<_>>()
            .join("\n");
        for field in [
            "Block information",
            "Label:",
            "Type:",
            "Flavor:",
            "Build terminal",
            "/work/project",
            "cargo (PID 42)",
            "needs input",
            "Approve deployment",
        ] {
            assert!(upper_text.contains(field), "missing {field}: {upper_text}");
        }
        let lower_text = (lower.y..lower.bottom())
            .map(|y| row(y, lower.x, lower.width))
            .collect::<String>();
        assert!(lower_text.contains("terminal output"));
        for (column, selected) in [(0, 0), (1, 1), (2, 1)] {
            let area = UiBlock::default()
                .borders(Borders::ALL)
                .inner(panes[column]);
            assert!(row(area.y + selected, area.x, area.width).contains("🙋"));
        }
        assert_eq!(
            session_state(&Snapshot::default(), browser.session().unwrap()),
            State::Unknown
        );
    }
    #[tokio::test]
    async fn metadata_load_scopes_sessions_and_releases_attachments() -> Result<()> {
        use crate::protocol::{Io, Socket, next_json};
        use futures_util::SinkExt;
        use tokio_tungstenite::{
            WebSocketStream,
            tungstenite::{Message, protocol::Role},
        };
        for scope in [None, Some("session:two")] {
            let (a, b) = tokio::io::duplex(65536);
            let a: Box<dyn Io> = Box::new(a);
            let b: Box<dyn Io> = Box::new(b);
            let client: Socket = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
            let mut server: Socket = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
            let server_task = async {
                next_json(&mut server).await?;
                server
                    .send(Message::text(
                        json!({"type":"welcome","api_revision":11}).to_string(),
                    ))
                    .await?;
                let request = next_json(&mut server).await?;
                assert_eq!(request["method"], "block_creator.list");
                server
                    .send(Message::text(
                        json!({"type":"response","id":request["id"],"payload":{"creators":[]}})
                            .to_string(),
                    ))
                    .await?;
                let request = next_json(&mut server).await?;
                assert_eq!(request["method"], "session.list");
                server.send(Message::text(json!({"type":"response","id":request["id"],"payload":{"sessions":[{"session_id":"session:one","label":"one"},{"session_id":"session:two","label":"two"}]}}).to_string())).await?;
                for session in if scope.is_some() {
                    vec!["session:two"]
                } else {
                    vec!["session:one", "session:two"]
                } {
                    for method in ["session.attach", "session.list_blocks", "session.detach"] {
                        let request = next_json(&mut server).await?;
                        assert_eq!(request["method"], method);
                        assert_eq!(request["payload"]["session_id"], session);
                        server.send(Message::text(json!({"type":"response","id":request["id"],"payload":if method=="session.attach" {json!({"view":{"windows":[]}})} else {json!({"blocks":[]})}}).to_string())).await?;
                    }
                }
                Ok::<_, anyhow::Error>(())
            };
            let client_task = async {
                let mut control = Control::hello(client).await?;
                let sessions = load(&mut control, scope).await?;
                assert_eq!(sessions.len(), if scope.is_some() { 1 } else { 2 });
                if let Some(scope) = scope {
                    assert!(sessions.iter().all(|s| s.id == scope));
                }
                Ok::<_, anyhow::Error>(())
            };
            tokio::try_join!(server_task, client_task)?;
        }
        Ok(())
    }
}
