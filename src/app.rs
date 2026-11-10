use crate::{
    actions, browser, domain::Target, help, preview, protocol, session::resize, shortcuts, stream,
    terminal, tty,
};
use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde_json::json;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

#[derive(Parser)]
#[command(version, about = "Attach to one terminal block on a local Rex server")]
struct Args {
    /// Rex Unix socket path (otherwise use REX_SERVER or discover a local server).
    #[arg(long)]
    socket: Option<String>,
    /// Session label, full ID, or hexadecimal ID suffix.
    #[arg(long)]
    session: Option<String>,
    /// Block label, full ID, or hexadecimal ID suffix within the session.
    #[arg(long, requires = "session")]
    block: Option<String>,
    /// Restore the initial screen and print it without entering interactive mode.
    #[arg(long, requires_all = ["session", "block"])]
    capture: bool,
    /// Attached terminal renderer.
    #[arg(long, value_enum, default_value = "ansi")]
    renderer: terminal::Backend,
    /// Compatibility flag: interactive attachment always takes ownership.
    #[arg(long = "take", hide = true, conflicts_with = "capture")]
    _take: bool,
    /// Browser preview: detect Kitty support, force Kitty, or use text.
    #[arg(long, value_enum, default_value = "auto")]
    preview: preview::Mode,
    /// Place browser info/preview in a column on the right or a row below.
    #[arg(long, value_enum)]
    preview_layout: Option<browser::PreviewLayout>,
    /// Monospace TTF/OTF font for image previews (otherwise use a system font).
    #[arg(long)]
    preview_font: Option<std::path::PathBuf>,
}

struct Config {
    args: Args,
    socket: String,
    shortcuts: shortcuts::Config,
}
impl std::ops::Deref for Config {
    type Target = Args;
    fn deref(&self) -> &Args {
        &self.args
    }
}

pub async fn run() -> Result<()> {
    let args = Args::parse();
    let shortcuts = shortcuts::Config::load()?;
    if !args.capture {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "interactive mode requires a terminal; use --capture for text output"
        );
        ensure!(
            std::env::var("TERM").as_deref() != Ok("dumb"),
            "TERM=dumb is unsupported"
        );
    }
    let (socket, mut control) = crate::discovery::connect(args.socket.as_deref()).await?;
    let args = Config {
        args,
        socket,
        shortcuts,
    };
    let scope = if let Some(session) = &args.session {
        Some(control.resolve_session(session).await?)
    } else {
        None
    };
    let mut selected = if let Some(block) = &args.block {
        let (session, block) = control
            .resolve_targets(scope.as_deref().unwrap(), block)
            .await?;
        Some(Target { session, block })
    } else {
        None
    };
    if args.capture {
        return attach(&args, selected.as_ref().unwrap(), &mut control, None)
            .await
            .map(|_| ());
    }
    let _guard = tty::Guard::enter().context("entering terminal mode")?;
    let mut input = tty::Input::open().context("opening terminal input")?;
    let options =
        preview::Options::detect(args.preview, args.preview_font.as_deref(), &mut input).await?;
    let mut notice = String::new();
    let mut previous = None;
    let mut browser = browser::Browser::new(
        options,
        args.preview_layout.unwrap_or(args.shortcuts.preview_layout),
        args.shortcuts.browser.clone(),
        args.shortcuts.terminal.clone(),
    );
    loop {
        let target = match selected.take() {
            Some(target) => target,
            None => match browser::browse(
                browser::Request {
                    socket: &args.socket,
                    control: &mut control,
                    scope: scope.as_deref(),
                    input: &mut input,
                    notice: &notice,
                    previous: previous.as_ref(),
                },
                &mut browser,
            )
            .await?
            {
                Some(target) => target,
                None => return Ok(()),
            },
        };
        previous = Some(target.clone());
        match attach(&args, &target, &mut control, Some(&mut input)).await {
            Ok(Flow::Detach) => notice.clear(),
            Ok(Flow::Exit) => return Ok(()),
            Ok(Flow::Switch(next)) => {
                selected = Some(next);
                notice.clear();
            }
            Err(error) => notice = format!("Could not attach: {error:#}"),
        }
    }
}

enum Flow {
    Detach,
    Exit,
    Switch(Target),
}

async fn attach(
    args: &Config,
    target: &Target,
    control: &mut protocol::Control,
    input: Option<&mut tty::Input>,
) -> Result<Flow> {
    crate::session::with_session(control, &target.session, async |control, attached| {
        // Detached blocks can still be attached, but have no window to focus.
        let placed = attached["view"]["windows"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|window| window["layers"].as_array().into_iter().flatten())
            .flat_map(|layer| layer["blocks"].as_array().into_iter().flatten())
            .any(|block| block["block_id"] == target.block);
        if !args.capture && placed {
            control
                .call(
                    "session.focus_block",
                    json!({"session_id":target.session,"block_id":target.block}),
                )
                .await?;
        }
        attach_inner(args, target, control, input).await
    })
    .await
}

async fn attach_inner(
    args: &Config,
    target: &Target,
    control: &mut protocol::Control,
    input: Option<&mut tty::Input>,
) -> Result<Flow> {
    let (mut data, mut state) =
        crate::session::open_terminal(control, &args.socket, target).await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while !state.is_ready() {
            state.feed(&stream::next_binary(&mut data).await?)?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("snapshot restoration timed out")??;
    if args.capture {
        print!(
            "{}",
            terminal::frame(state.terminal_mut()?, u16::MAX, u16::MAX, false)?
        );
        return Ok(Flow::Exit);
    }
    control
        .call(
            "session.own_block",
            json!({"session_id":target.session,"block_ids":[target.block],"mode":"take"}),
        )
        .await?;
    interactive(
        target,
        control,
        &mut data,
        &mut state,
        input.context("missing interactive input")?,
        &args.shortcuts,
        args.renderer,
    )
    .await
}

async fn interactive(
    target: &Target,
    control: &mut protocol::Control,
    data: &mut protocol::Socket,
    state: &mut terminal::State,
    input_tty: &mut tty::Input,
    config: &shortcuts::Config,
    backend: terminal::Backend,
) -> Result<Flow> {
    let bindings = &config.terminal;
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut renderer = terminal::Renderer::new(backend)?;
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut dimensions = tty::size()?;
    resize(target, control, dimensions).await?;
    let mut dirty = true;
    let mut input = Vec::new();
    let mut keys = shortcuts::Matcher::new(bindings);
    let mut key_deadline = None;
    let mut bytes = [0u8; 4096];
    loop {
        let events = tokio::select! {
            bytes = stream::next_binary(data) => { state.feed(&bytes?)?; dirty = true; None }
            value = control.next_event() => { let _ = value?; None }
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            _ = winch.recv() => { dimensions = tty::size()?; resize(target,control,dimensions).await?; dirty = true; None }
            n = input_tty.read(&mut bytes) => {
                let n = n.context("reading terminal input")?;
                if n == 0 { break; }
                let events = keys.feed(&bytes[..n]);
                key_deadline = keys.waiting().then(||tokio::time::Instant::now()+keys.delay());
                Some(events)
            }
            _ = tokio::time::sleep_until(key_deadline.unwrap_or_else(||tokio::time::Instant::now()+Duration::from_secs(3600))), if key_deadline.is_some() => {
                key_deadline=None;
                let events=keys.timeout();
                Some(events)
            }
            _ = tick.tick(), if dirty => {
                if state.render_held(std::time::Instant::now())? { continue; }
                draw_terminal(&mut renderer, state, dimensions)?;
                dirty = false;
                None
            }
        };
        if let Some(events) = events
            && let Some(action) = crate::session::while_streaming(
                terminal_events(events, &mut keys, &mut input, target, control, input_tty),
                data,
                state,
                |state| draw_terminal(&mut renderer, state, dimensions),
            )
            .await?
        {
            match action {
                TerminalAction::Navigate(flow) => return Ok(flow),
                TerminalAction::Help => {
                    if !help::show_terminal(config, input_tty, data, state, control, target).await?
                    {
                        return Ok(Flow::Exit);
                    }
                    renderer = terminal::Renderer::new(backend)?;
                    dimensions = tty::size()?;
                    dirty = true;
                    keys = shortcuts::Matcher::new(bindings);
                    key_deadline = None;
                }
            }
        }
    }
    Ok(Flow::Exit)
}

fn draw_terminal(
    renderer: &mut terminal::Renderer,
    state: &mut terminal::State,
    size: (u16, u16),
) -> Result<()> {
    let frame = renderer.frame(state.terminal()?, size.0, size.1)?;
    if !frame.is_empty() {
        let mut out = io::stdout().lock();
        out.write_all(frame.as_bytes())
            .context("drawing terminal screen")?;
        out.flush().context("flushing terminal screen")?;
    }
    Ok(())
}

enum TerminalAction {
    Navigate(Flow),
    Help,
}

async fn terminal_events(
    events: Vec<shortcuts::Event>,
    keys: &mut shortcuts::Matcher,
    input: &mut Vec<u8>,
    target: &Target,
    control: &mut protocol::Control,
    tty: &mut tty::Input,
) -> Result<Option<TerminalAction>> {
    let mut events = events.into_iter();
    while let Some(event) = events.next() {
        match event {
            shortcuts::Event::Raw(bytes) => input.extend(bytes),
            shortcuts::Event::Action(action, _) if action == "client.send_prefix" => input.push(2),
            shortcuts::Event::Action(action, _) => {
                send_input(input, target, control).await?;
                let flow = if action == "client.shortcuts" {
                    TerminalAction::Help
                } else if action == "client.detach" {
                    TerminalAction::Navigate(Flow::Detach)
                } else {
                    TerminalAction::Navigate(Flow::Switch(
                        actions::terminal_action(control, target, action).await?,
                    ))
                };
                let mut rest: Vec<u8> = events.flat_map(shortcuts::Event::bytes).collect();
                rest.extend(keys.remaining());
                tty.unread(&rest);
                return Ok(Some(flow));
            }
        }
    }
    send_input(input, target, control).await?;
    Ok(None)
}
async fn send_input(
    input: &mut Vec<u8>,
    target: &Target,
    control: &mut protocol::Control,
) -> Result<()> {
    let valid = match std::str::from_utf8(input) {
        Ok(_) => input.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(e) => return Err(e.into()),
    };
    if valid > 0 {
        let text = std::str::from_utf8(&input[..valid])?;
        control
            .call(
                "com.superlogical.terminal.write",
                json!({"session_id":target.session,"block_id":target.block,"args":{"data":text}}),
            )
            .await?;
        input.drain(..valid);
    }
    Ok(())
}

#[cfg(test)]
mod attach_tests {
    use super::*;
    use crate::protocol::{Io, Socket, next_json};
    use futures_util::SinkExt;
    use tokio_tungstenite::{
        WebSocketStream,
        tungstenite::{Message, protocol::Role},
    };

    #[tokio::test]
    async fn interactive_attach_focuses_placed_blocks_and_cleans_up_failures() -> Result<()> {
        for (capture, placed) in [(false, true), (true, true), (false, false)] {
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
                let mut methods = vec!["session.attach"];
                if !capture && placed {
                    methods.push("session.focus_block");
                }
                methods.extend(["data.connect", "session.detach"]);
                for method in methods {
                    let request = next_json(&mut server).await?;
                    assert_eq!(request["method"], method);
                    assert_eq!(request["payload"]["session_id"], "session:test");
                    if method == "session.focus_block" {
                        assert_eq!(request["payload"]["block_id"], "block:test");
                    }
                    let reply = if method == "data.connect" {
                        json!({"type":"response","id":request["id"],"error":{"code":"not_found","message":"test data unavailable"}})
                    } else {
                        let payload = if method == "session.attach" && placed {
                            json!({"view":{"windows":[{"layers":[{"blocks":[{"block_id":"block:test"}]}]}]}})
                        } else {
                            json!({})
                        };
                        json!({"type":"response","id":request["id"],"payload":payload})
                    };
                    server.send(Message::text(reply.to_string())).await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            let client_task = async {
                let mut control = protocol::Control::hello(client).await?;
                let mut cli = Args::parse_from(["rex-attach", "--socket", "/unused"]);
                cli.capture = capture;
                let args = Config {
                    args: cli,
                    socket: "/unused".into(),
                    shortcuts: shortcuts::Config::load()?,
                };
                let target = Target {
                    session: "session:test".into(),
                    block: "block:test".into(),
                };
                assert!(attach(&args, &target, &mut control, None).await.is_err());
                Ok::<_, anyhow::Error>(())
            };
            tokio::try_join!(server_task, client_task)?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn fragmented_utf8_is_buffered_and_forwarded_once() -> Result<()> {
        let (mut control, mut server) = crate::test_support::control_pair().await?;
        let target = Target {
            session: "s".into(),
            block: "b".into(),
        };
        let serve = async {
            for expected in ["h", "界"] {
                let request = protocol::next_json(&mut server).await?;
                assert_eq!(request["payload"]["args"]["data"], expected);
                crate::test_support::reply(&mut server, &request, json!({}), false).await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let client = async {
            let mut pending = vec![b'h', 0xe7];
            send_input(&mut pending, &target, &mut control).await?;
            assert_eq!(pending, vec![0xe7]);
            pending.extend([0x95, 0x8c]);
            send_input(&mut pending, &target, &mut control).await?;
            assert!(pending.is_empty());
            pending.push(0xff);
            assert!(
                send_input(&mut pending, &target, &mut control)
                    .await
                    .is_err()
            );
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(serve, client)?;
        Ok(())
    }
}
