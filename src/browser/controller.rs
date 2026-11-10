use super::{
    state::*,
    view::{Screen, preview_size},
};
use crate::{
    activity::Inspector,
    domain::Target,
    preview::Preview,
    protocol::Control,
    shortcuts::{Event, Matcher},
    tty,
};
use anyhow::Result;
use std::time::Duration;
pub struct Request<'a> {
    pub socket: &'a str,
    pub control: &'a mut Control,
    pub scope: Option<&'a str>,
    pub input: &'a mut tty::Input,
    pub notice: &'a str,
    pub previous: Option<&'a Target>,
}
pub async fn browse(request: Request<'_>, browser: &mut Browser) -> Result<Option<Target>> {
    let Request { socket, scope, .. } = &request;
    let scope = *scope;
    if browser.sessions.is_empty() && scope.is_some() {
        browser.focus = Focus::Windows;
    }
    let mut data = super::data::Data::start(socket, scope);
    let dimensions = tty::size()?;
    let size = preview_size(browser, dimensions);
    let target = preview_target(browser, size);
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
    let result = browse_inner(request, browser, &mut pane, &mut data).await;
    data.stop().await;
    pane.worker.stop().await;
    pane.inspector.stop().await;
    result
}

pub(super) struct Pane {
    pub(super) inspector: Inspector,
    pub(super) worker: Preview,
    pub(super) target: Option<Target>,
    pub(super) size: (u16, u16),
    pub(super) open: bool,
}

async fn browse_inner(
    request: Request<'_>,
    browser: &mut Browser,
    pane: &mut Pane,
    data: &mut super::data::Data,
) -> Result<Option<Target>> {
    let _mouse = tty::MouseGuard::enter(false)?;
    let Request {
        socket,
        control,
        input,
        notice,
        previous,
        ..
    } = request;
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
    let mut data_open = true;
    let mut restore = previous.cloned();
    loop {
        pane.update(socket, browser, dimensions).await;
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
            _=tick.tick()=> {data.refresh();vec![]},
            result=data.changed(), if data_open => {
                match result {
                    Some(Ok(loaded)) => {
                        browser.replace(loaded.sessions);
                        if let Some(target) = restore.take() { browser.restore(&target); }
                        if let Some(id) = loaded.created { browser.select_id(&id); notice = format!("Created {id}"); }
                    }
                    Some(Err(error)) => notice = error,
                    None => data_open = false,
                }
                vec![]
            },
            _=winch.recv()=> {dimensions=tty::size()?;vec![]},
            _=interrupt.recv()=>return Ok(None),_=terminate.recv()=>return Ok(None),
            value=control.next_event()=> {value?;vec![]},
        };
        let before = browser.target();
        if let Some(target) = dispatch(
            actions,
            browser,
            data,
            &mut notice,
            &mut keys,
            input,
            &mut screen,
        )? {
            return Ok(target);
        }
        if browser.target() != before {
            restore = None;
        }
    }
}

fn dispatch(
    actions: Vec<Event>,
    browser: &mut Browser,
    data: &mut super::data::Data,
    notice: &mut String,
    keys: &mut Matcher,
    input: &mut tty::Input,
    screen: &mut Screen,
) -> Result<Option<Option<Target>>> {
    let mut events = actions.into_iter();
    while let Some(event) = events.next() {
        let effect = match match event {
            Event::Mouse(mouse) => screen.mouse(browser, mouse),
            event => browser.feed(event),
        } {
            Ok(effect) => effect,
            Err(error) => {
                *notice = format!("{error:#}");
                continue;
            }
        };
        match effect {
            Effect::None => {}
            Effect::Exit => return Ok(Some(None)),
            Effect::Attach(target) => {
                let mut rest: Vec<u8> = events.flat_map(Event::bytes).collect();
                rest.extend(keys.remaining());
                input.unread(&rest);
                return Ok(Some(Some(target)));
            }
            Effect::Refresh => data.refresh(),
            Effect::Mutate(dialog) => match data.mutate(dialog) {
                None => notice.clear(),
                Some(dialog) => {
                    browser.dialog = Some(dialog);
                    *notice = "Browser operation in progress; try again when it completes.".into();
                }
            },
        }
    }
    Ok(None)
}

impl Pane {
    async fn update(&mut self, socket: &str, browser: &Browser, dimensions: (u16, u16)) {
        let size = preview_size(browser, dimensions);
        let target = preview_target(browser, size);
        if self.target != target || self.size != size {
            let old = std::mem::replace(
                &mut self.worker,
                Preview::start(
                    socket,
                    target.clone(),
                    size.0,
                    size.1,
                    browser.preview_options.clone(),
                ),
            );
            old.stop().await;
            self.target = target;
            self.size = size;
            self.open = true;
        }
        self.inspector.update(browser.inspection());
    }
}

fn preview_target(browser: &Browser, size: (u16, u16)) -> Option<Target> {
    if size.0 == 0 || size.1 == 0 {
        None
    } else {
        browser.target()
    }
}
