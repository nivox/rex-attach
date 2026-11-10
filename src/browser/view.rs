use super::{dialog::*, model::*, state::*};
use crate::{
    activity::{self, Snapshot, State},
    domain::Target,
    text::clean,
};
use anyhow::Result;
use ratatui::{
    Frame as UiFrame, Terminal as UiTerminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as UiBlock, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    io::{self, Write},
    sync::Arc,
};
pub(super) struct Screen {
    pub(super) terminal: UiTerminal<CrosstermBackend<io::Stdout>>,
    pub(super) image: Option<Arc<crate::preview::RenderedImage>>,
    hits: Vec<Hit>,
    click: Option<(Focus, usize, Option<Target>, std::time::Instant)>,
}
pub(super) struct Hit {
    area: Rect,
    focus: Focus,
    offset: usize,
    len: usize,
}
impl Hit {
    pub(super) fn at(&self, mouse: crate::mouse::Mouse) -> Option<(Focus, usize)> {
        if self.len == 0 || !self.area.contains((mouse.x, mouse.y).into()) {
            return None;
        }
        let row = self.offset + usize::from(mouse.y - self.area.y);
        if mouse.wheel().is_some() {
            Some((self.focus, row.min(self.len - 1)))
        } else {
            (row < self.len).then_some((self.focus, row))
        }
    }
}
impl Screen {
    pub(super) fn mouse(
        &mut self,
        browser: &mut Browser,
        mouse: crate::mouse::Mouse,
    ) -> Result<Effect> {
        let hit = self.hits.iter().find_map(|h| h.at(mouse));
        let now = std::time::Instant::now();
        let activate = mouse.left_press()
            && hit.is_some_and(|(focus, row)| {
                self.click.as_ref().is_some_and(|(f, r, target, time)| {
                    *f == focus
                        && *r == row
                        && *target == browser.target()
                        && now.duration_since(*time) < std::time::Duration::from_millis(400)
                })
            });
        if mouse.left_press() || mouse.wheel().is_some() {
            self.click = None;
        }
        let effect = browser.mouse(mouse, hit, activate)?;
        if mouse.left_press() && !activate && browser.dialog.is_none() && browser.help.is_none() {
            self.click = hit.map(|(f, row)| (f, row, browser.target(), now));
        }
        Ok(effect)
    }
    pub(super) fn new() -> Result<Self> {
        // Our outer Guard owns raw mode and the alternate screen. Ratatui owns
        // only browser drawing, so attaching never nests terminal setup.
        let mut terminal = UiTerminal::new(CrosstermBackend::new(io::stdout()))?;
        // Fullscreen setup needs no cursor-position query. Avoid clear(),
        // which asks the backend to read stdin and would compete with Input.
        terminal.backend_mut().write_all(b"\x1b[2J\x1b[H")?;
        Ok(Self {
            terminal,
            image: None,
            hits: vec![],
            click: None,
        })
    }
    pub(super) fn draw(
        &mut self,
        browser: &Browser,
        preview: &crate::preview::Frame,
        snapshot: &Snapshot,
        notice: &str,
    ) -> Result<()> {
        let image = preview.image.as_ref().filter(|_| !browser.preview_hidden);
        if self.image.as_ref().map(|i| i.id) != image.map(|i| i.id)
            && let Some(image) = self.image.take()
        {
            image.delete(self.terminal.backend_mut())?;
        }
        self.image = image.cloned();
        self.terminal.backend_mut().write_all(b"\x1b[?2026h")?;
        let result = self
            .terminal
            .draw(|frame| self.hits = render(frame, browser, preview, snapshot, notice))
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

pub(super) fn pane_rects(browser: &Browser, area: Rect) -> Vec<Rect> {
    let mut body = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(2),
    );
    let mut details = None;
    if !browser.preview_hidden && browser.preview_layout == PreviewLayout::Row {
        let [upper, lower] = right_rects(body);
        body = upper;
        details = Some(lower);
    }
    let sides = if browser.show_blocks() { 3 } else { 2 };
    let count = sides
        + usize::from(!browser.preview_hidden && browser.preview_layout == PreviewLayout::Column);
    let side = (body.width / count as u16).max(1);
    let mut constraints = vec![Constraint::Length(side); count - 1];
    constraints.push(Constraint::Min(1));
    let mut panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(body)
        .to_vec();
    panes.extend(details);
    panes
}
pub(super) fn preview_size(browser: &Browser, size: (u16, u16)) -> (u16, u16) {
    if browser.preview_hidden || size.0 < 40 || size.1 < 8 {
        return (0, 0);
    }
    let panes = pane_rects(browser, Rect::new(0, 0, size.0, size.1));
    let area = UiBlock::default()
        .borders(Borders::ALL)
        .inner(detail_rects(browser, *panes.last().unwrap())[1]);
    (area.width, area.height)
}
pub(super) fn right_rects(pane: Rect) -> [Rect; 2] {
    Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(pane)
}
pub(super) fn detail_rects(browser: &Browser, pane: Rect) -> [Rect; 2] {
    match browser.preview_layout {
        PreviewLayout::Column => right_rects(pane),
        PreviewLayout::Row => {
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(pane)
        }
    }
}
pub(super) fn block_state(snapshot: &Snapshot, session: &str, block: &Block) -> State {
    snapshot.state(&Target {
        session: session.into(),
        block: block.id.clone(),
    })
}
pub(super) fn window_state(snapshot: &Snapshot, session: &str, window: &Window) -> State {
    activity::aggregate(
        window
            .blocks
            .iter()
            .map(|b| block_state(snapshot, session, b)),
    )
}
pub(super) fn session_state(snapshot: &Snapshot, session: &Session) -> State {
    activity::aggregate(
        session
            .windows
            .iter()
            .map(|t| window_state(snapshot, &session.id, t)),
    )
}
pub(super) fn entry(state: State, label: String) -> ListItem<'static> {
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
pub(super) fn information(frame: &mut UiFrame, browser: &Browser, snapshot: &Snapshot, area: Rect) {
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

pub(super) fn render(
    frame: &mut UiFrame,
    browser: &Browser,
    preview: &crate::preview::Frame,
    snapshot: &Snapshot,
    notice: &str,
) -> Vec<Hit> {
    if let Some(help) = &browser.help {
        help.render(frame, &browser.bindings, &browser.terminal_bindings);
        return vec![];
    }
    let area = frame.area();
    if area.is_empty() {
        return vec![];
    }
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(
                "Arrows/hjkl: navigate | Enter: attach | c: create | r: rename | x: close | p: info/preview | ?: help | q/Esc: exit",
            ),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
    if area.width < 40 {
        frame.render_widget(
            Paragraph::new("Widen terminal to browse (40+ cols). Esc: exit"),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
        return vec![];
    }
    let panes = pane_rects(browser, area);
    let hits = render_columns(frame, browser, snapshot, &panes);
    if !browser.preview_hidden {
        let [info_area, pane] = detail_rects(browser, *panes.last().unwrap());
        information(frame, browser, snapshot, info_area);
        render_preview(frame, preview, browser.preview_options.images(), pane);
    }
    render_footer(frame, browser, notice);
    if let Some(dialog) = &browser.dialog {
        dialog.render(frame);
    }
    hits
}

impl Dialog {
    pub(super) fn render(&self, frame: &mut UiFrame) {
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

type Column = (&'static str, Focus, usize, Vec<ListItem<'static>>);
fn columns(browser: &Browser, snapshot: &Snapshot) -> Vec<Column> {
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
    columns
}
fn render_columns(
    frame: &mut UiFrame,
    browser: &Browser,
    snapshot: &Snapshot,
    panes: &[Rect],
) -> Vec<Hit> {
    let mut hits = vec![];
    for (index, (title, focus, selected, items)) in
        columns(browser, snapshot).into_iter().enumerate()
    {
        let active = browser.focus == focus;
        let border = UiBlock::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }));
        let len = items.len();
        let inner = border.inner(panes[index]);
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
        hits.push(Hit {
            area: inner,
            focus,
            offset: selected.offset(),
            len,
        });
    }
    hits
}
fn render_preview(frame: &mut UiFrame, preview: &crate::preview::Frame, images: bool, pane: Rect) {
    let title = if images {
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
        // Kitty placeholders encode their image ID in the foreground RGB
        // value. Crossterm's NO_COLOR handling would erase that address and
        // leave a successfully transmitted image with no visible placement.
        ratatui::crossterm::style::force_color_output(true);
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
}
fn render_footer(frame: &mut UiFrame, browser: &Browser, notice: &str) {
    let area = frame.area();
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
}
