//! Shortcut help displays the effective map rather than a copy of the defaults.
use crate::{
    Target, protocol,
    shortcuts::{Bindings, Event, Matcher},
    stream, terminal, tty,
};
use anyhow::Result;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Row, Table},
};
use std::{
    io::{self, Write},
    time::Duration,
};

#[derive(Default)]
pub struct Help {
    offset: usize,
    pasting: bool,
    page_rows: std::cell::Cell<usize>,
}
pub fn row_count(browser: &Bindings, terminal: &Bindings) -> usize {
    browser.display.len() + terminal.display.len() + 2
}
impl Help {
    /// Help is modal: navigation/close only, with no application actions or input.
    pub fn feed(&mut self, event: Event, count: usize) -> bool {
        let (action, bytes) = match event {
            Event::Action(a, b) => (a, b),
            Event::Raw(b) => (String::new(), b),
        };
        if bytes == b"\x1b[200~" {
            self.pasting = true;
            return false;
        }
        if bytes == b"\x1b[201~" {
            self.pasting = false;
            return false;
        }
        if self.pasting {
            return false;
        }
        if action == "client.shortcuts" {
            return true;
        }
        match bytes.as_slice() {
            b"\x1b" | b"q" | b"?" | b"\r" | b"\n" | b"\x03" => return true,
            b"j" | b"\x1b[B" | b"\x1bOB" => self.offset = self.offset.saturating_add(1),
            b"k" | b"\x1b[A" | b"\x1bOA" => self.offset = self.offset.saturating_sub(1),
            b"\x1b[6~" | b"\x04" => self.offset = self.offset.saturating_add(10),
            b"\x1b[5~" | b"\x15" => self.offset = self.offset.saturating_sub(10),
            b"g" | b"\x1b[H" | b"\x1bOH" => self.offset = 0,
            b"G" | b"\x1b[F" | b"\x1bOF" => self.offset = count.saturating_sub(1),
            _ => {}
        }
        self.offset = self
            .offset
            .min(count.saturating_sub(self.page_rows.get().max(1)));
        false
    }
    pub fn render(&self, frame: &mut Frame, browser: &Bindings, terminal: &Bindings) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let border = Block::default()
            .title("Shortcut help")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let inner = border.inner(area);
        frame.render_widget(border, area);
        let parts = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new("Configured bindings · > means press the next chord"),
            parts[0],
        );
        let height = parts[1].height.saturating_sub(1) as usize;
        self.page_rows.set(height);
        let count = row_count(browser, terminal);
        let offset = self.offset.min(count.saturating_sub(height.max(1)));
        let rows = [("Browser", browser), ("Terminal", terminal)]
            .into_iter()
            .flat_map(|(section, bindings)| {
                std::iter::once(
                    Row::new([section.to_owned(), String::new(), String::new()]).style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .chain(bindings.display.iter().map(|(key, action)| {
                    Row::new([key.clone(), description(action).into(), action.clone()])
                }))
            })
            .skip(offset);
        frame.render_widget(
            Table::new(
                rows,
                [
                    Constraint::Percentage(28),
                    Constraint::Percentage(32),
                    Constraint::Percentage(40),
                ],
            )
            .header(
                Row::new(["Key", "Action", "Identifier"]).style(Style::default().fg(Color::Yellow)),
            ),
            parts[1],
        );
        let end = (offset + height).min(count);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!(
                    "{}–{} of {} · ↑/↓ j/k: scroll · PgUp/PgDn · g/G: first/last",
                    if end == 0 { 0 } else { offset + 1 },
                    end,
                    count
                )),
                Line::from("Esc / q / Enter / ?: close help"),
            ]),
            parts[2],
        );
    }
}
fn description(action: &str) -> &str {
    match action {
        "client.shortcuts" => "Show shortcut help",
        "client.create" => "Create focused kind",
        "client.rename" => "Rename focused entry",
        "client.close" => "Close focused entry (confirm)",
        "client.browser.up" => "Previous entry",
        "client.browser.down" => "Next entry",
        "client.browser.left" => "Parent column",
        "client.browser.right" => "Child column",
        "client.browser.choose" => "Choose / attach",
        "client.browser.exit" => "Exit browser",
        "session.refresh" => "Refresh browser",
        "session.new" => "Create session",
        "session.rename" => "Rename session",
        "session.close" => "Close session (confirm)",
        "client.tab.new" => "Create tab",
        "client.tab.rename" => "Rename tab",
        "client.tab.close" => "Close tab (confirm)",
        "client.tab.next" => "Attach next tab",
        "client.tab.previous" => "Attach previous tab",
        "pane.split" | "pane.split.right" => "Split side by side",
        "pane.split.down" => "Split above / below",
        "pane.focus_next" => "Attach next block",
        "pane.rename" => "Rename block",
        "pane.close" => "Close block (confirm)",
        "client.detach" => "Return to browser",
        "client.send_prefix" => "Send literal Ctrl-B",
        _ => action,
    }
}

/// Keep consuming the live stream while help covers the attached terminal.
/// Returns false if the program should exit, true when help was dismissed.
pub async fn show_terminal(
    config: &crate::shortcuts::Config,
    input: &mut tty::Input,
    data: &mut protocol::Socket,
    state: &mut terminal::State,
    control: &mut protocol::Control,
    target: &Target,
) -> Result<bool> {
    let bindings = &config.terminal;
    let mut screen = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    screen.backend_mut().write_all(b"\x1b[0m\x1b[2J\x1b[H")?;
    let mut help = Help::default();
    let mut keys = Matcher::new(bindings);
    let mut deadline = None;
    let mut bytes = [0u8; 4096];
    let mut dirty = true;
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        if dirty {
            screen.draw(|f| help.render(f, &config.browser, bindings))?;
            dirty = false;
        }
        let events = tokio::select! {
            result=input.read(&mut bytes)=>{let n=result?;if n==0{return Ok(false);}let events=keys.feed(&bytes[..n]);deadline=keys.waiting().then(||tokio::time::Instant::now()+keys.delay());events},
            _=tokio::time::sleep_until(deadline.unwrap_or_else(||tokio::time::Instant::now()+Duration::from_secs(3600))),if deadline.is_some()=>{deadline=None;keys.timeout()},
            result=stream::next_binary(data)=>{state.feed(&result?)?;vec![]},
            value=protocol::next_json(&mut control.socket)=>{value?;vec![]},
            _=winch.recv()=>{crate::resize(target,control,tty::size()?).await?;dirty=true;vec![]},
            _=interrupt.recv()=>return Ok(false),_=terminate.recv()=>return Ok(false),
        };
        for event in events {
            dirty = true;
            if help.feed(event, row_count(&config.browser, bindings)) {
                return Ok(true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn help_shows_effective_bindings_scrolls_and_swallows_actions() {
        let mut bindings = Bindings::default();
        bindings.display = (0..40)
            .map(|i| (format!("key{i:02}"), "client.rename".into()))
            .collect();
        let mut terminal = Bindings::default();
        terminal.display = vec![("ctrl+b>d".into(), "client.detach".into())];
        let count = row_count(&bindings, &terminal);
        let mut help = Help::default();
        let mut screen = Terminal::new(ratatui::backend::TestBackend::new(120, 12)).unwrap();
        screen
            .draw(|f| help.render(f, &bindings, &terminal))
            .unwrap();
        let text = |screen: &Terminal<ratatui::backend::TestBackend>| {
            screen
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        assert!(text(&screen).contains("key00"));
        assert!(text(&screen).contains("client.rename"));
        assert!(text(&screen).contains("Browser"));
        assert!(!help.feed(Event::Action("client.close".into(), b"x".to_vec()), count));
        assert!(!help.feed(Event::Raw(b"G".to_vec()), count));
        screen
            .draw(|f| help.render(f, &bindings, &terminal))
            .unwrap();
        assert!(text(&screen).contains("Terminal"));
        assert!(text(&screen).contains("ctrl+b>d"));
        help.feed(Event::Raw(b"k".to_vec()), count);
        screen
            .draw(|f| help.render(f, &bindings, &terminal))
            .unwrap();
        assert!(!text(&screen).contains("ctrl+b>d"));
        help.feed(Event::Raw(b"\x1b[200~".to_vec()), 40);
        assert!(!help.feed(Event::Raw(b"q".to_vec()), 40));
        help.feed(Event::Raw(b"\x1b[201~".to_vec()), 40);
        assert!(help.feed(Event::Raw(b"\x1b".to_vec()), 40));
        for (width, height) in [(1, 1), (20, 4), (40, 8)] {
            let mut screen =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            screen
                .draw(|f| help.render(f, &bindings, &terminal))
                .unwrap();
        }
    }
}
