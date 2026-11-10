use super::{data::load, model::*, state::*, view::*};
use crate::{
    activity::{self, Snapshot, State},
    domain::Selection,
    domain::Target,
    protocol::Control,
    text::clean,
};
use anyhow::Result;
use ratatui::{
    Terminal as UiTerminal,
    layout::Rect,
    widgets::{Block as UiBlock, Borders},
};
use serde_json::json;

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
    let mut terminal = UiTerminal::new(ratatui::backend::TestBackend::new(size.0, size.1)).unwrap();
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
            );
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
fn kitty_preview_has_visible_placements_in_ghostty() -> Result<()> {
    use ratatui::{TerminalOptions, Viewport, backend::CrosstermBackend, layout::Size};
    use ratatui_image::protocol::{Protocol, kitty::Kitty};
    use std::sync::Arc;
    // Crossterm caches color policy globally. Isolate this case from terminal
    // renderer tests that also enable protocol colors in the same process.
    const CHILD: &str = "REX_ATTACH_TEST_KITTY_NO_COLOR";
    if std::env::var_os(CHILD).is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "browser::tests::kitty_preview_has_visible_placements_in_ghostty",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("NO_COLOR", "1")
            .status()?;
        assert!(
            status.success(),
            "Kitty placement check failed with NO_COLOR"
        );
        return Ok(());
    }
    for layout in [PreviewLayout::Column, PreviewLayout::Row] {
        let browser = Browser {
            preview_layout: layout,
            ..Default::default()
        };
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            20,
            20,
            image::Rgba([255, 0, 0, 255]),
        ));
        let preview = crate::preview::Frame {
            text: String::new(),
            image: Some(Arc::new(crate::preview::RenderedImage {
                protocol: Protocol::Kitty(Kitty::new(
                    image,
                    Size::new(2, 1),
                    0x12345678,
                    false,
                    false,
                )?),
                id: 0x12345678,
                tmux: false,
            })),
        };
        let mut output = Vec::new();
        {
            let mut terminal = UiTerminal::with_options(
                CrosstermBackend::new(&mut output),
                TerminalOptions {
                    viewport: Viewport::Fixed(Rect::new(0, 0, 120, 40)),
                },
            )?;
            terminal.draw(|frame| {
                render(
                    frame,
                    &browser,
                    &crate::preview::Frame::default(),
                    &Snapshot::default(),
                    "",
                );
            })?;
            terminal.draw(|frame| {
                render(frame, &browser, &preview, &Snapshot::default(), "");
            })?;
            terminal.draw(|frame| {
                render(frame, &browser, &preview, &Snapshot::default(), "");
            })?;
        }
        assert!(
            visible_images(&output) > 0,
            "no visible image in {layout:?} layout"
        );
    }
    Ok(())
}

fn visible_images(output: &[u8]) -> usize {
    use ghostty_vt_sys as ffi;
    // Each handle is created here, used synchronously, then released below.
    unsafe {
        let mut terminal = std::ptr::null_mut();
        let mut state = std::ptr::null_mut();
        let mut iterator = std::ptr::null_mut();
        assert_eq!(
            ffi::ghostty_terminal_new(std::ptr::null(), &mut terminal, 120, 40),
            0
        );
        assert_eq!(ffi::ghostty_terminal_resize(terminal, 120, 40, 10, 20), 0);
        let limit = 64 * 1024 * 1024u64;
        assert_eq!(
            ffi::ghostty_terminal_set(
                terminal,
                ffi::TerminalOption::KITTY_IMAGE_STORAGE_LIMIT,
                std::ptr::from_ref(&limit).cast()
            ),
            0
        );
        ffi::ghostty_terminal_vt_write(terminal, output.as_ptr(), output.len());
        assert_eq!(
            ffi::ghostty_render_state_new(std::ptr::null(), &mut state),
            0
        );
        assert_eq!(ffi::ghostty_render_state_update(state, terminal), 0);
        assert_eq!(
            ffi::ghostty_render_state_image_iterator_new(std::ptr::null(), &mut iterator),
            0
        );
        assert_eq!(
            ffi::ghostty_render_state_get(
                state,
                ffi::RenderStateData::IMAGE_ITERATOR,
                std::ptr::from_mut(&mut iterator).cast()
            ),
            0
        );
        let mut count = 0;
        while ffi::ghostty_render_state_image_iterator_next(iterator) {
            count += 1;
        }
        ffi::ghostty_render_state_image_iterator_free(iterator);
        ffi::ghostty_render_state_free(state);
        ffi::ghostty_terminal_free(terminal);
        count
    }
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
fn row_layout_and_toggle_reclaim_space_without_changing_selection() -> Result<()> {
    let config = crate::shortcuts::Config::parse(serde_json::Value::Null)?;
    let mut keys = crate::shortcuts::Matcher::new(&config.browser);
    let mut browser = Browser {
        sessions: vec![fixture()],
        window: 1,
        preview_layout: PreviewLayout::Row,
        ..Default::default()
    };
    let target = browser.target();
    let area = Rect::new(0, 0, 120, 40);
    let panes = pane_rects(&browser, area);
    assert_eq!(panes.len(), 4);
    assert!(panes[..3].iter().all(|p| p.width == 40 && p.height == 19));
    let [info, preview] = detail_rects(&browser, panes[3]);
    assert_eq!(info, Rect::new(0, 20, 60, 19));
    assert_eq!(preview, Rect::new(60, 20, 60, 19));
    assert_eq!(preview_size(&browser, (120, 40)), (58, 17));
    for layout in [PreviewLayout::Column, PreviewLayout::Row] {
        browser.preview_layout = layout;
        assert!(test_screen(&browser, (120, 40), "output").contains("Preview (text)"));
        for event in keys.feed(b"p") {
            browser.feed(event)?;
        }
        assert!(browser.preview_hidden);
        assert_eq!(browser.target(), target);
        let panes = pane_rects(&browser, area);
        assert_eq!(panes.len(), 3);
        assert!(panes.iter().all(|p| p.width == 40 && p.height == 38));
        assert_eq!(preview_size(&browser, (120, 40)), (0, 0));
        let screen = test_screen(&browser, (120, 40), "output");
        assert!(!screen.contains("Preview (text)"));
        assert!(!screen.contains("Block information"));
        for event in keys.feed(b"p") {
            browser.feed(event)?;
        }
        assert!(!browser.preview_hidden);
        for width in [1, 20, 40, 80] {
            for height in [1, 3, 8, 24] {
                test_screen(&browser, (width, height), "");
            }
        }
    }
    browser.dialog = Some(browser.start_dialog("client.rename")?);
    for event in keys.feed(b"p") {
        browser.feed(event)?;
    }
    assert!(!browser.preview_hidden);
    assert!(browser.dialog.as_ref().unwrap().text.ends_with('p'));
    Ok(())
}
#[test]
fn compact_browser_uses_configured_footer_and_equal_column_widths() {
    let mut browser = Browser {
        preview_layout: PreviewLayout::Column,
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
        screen
            .lines()
            .last()
            .unwrap()
            .starts_with("Detach from a block with Ctrl-A d | Help when attached with Ctrl-A ?")
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
        preview_layout: PreviewLayout::Column,
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
            );
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

#[test]
fn pure_transitions_keep_help_and_dialogs_modal() -> Result<()> {
    use crate::shortcuts::Event;
    let mut browser = Browser {
        sessions: vec![fixture()],
        ..Default::default()
    };
    let action = |name: &str| Event::Action(name.into(), vec![]);
    assert!(matches!(
        browser.feed(action("client.shortcuts"))?,
        Effect::None
    ));
    browser.feed(action("client.close"))?;
    assert!(browser.dialog.is_none());
    browser.feed(Event::Raw(b"q".to_vec()))?;
    assert!(browser.help.is_none());
    browser.feed(action("client.rename"))?;
    assert!(browser.dialog.is_some());
    assert!(matches!(
        browser.feed(Event::Raw(b"\x1b".to_vec()))?,
        Effect::None
    ));
    assert!(browser.dialog.is_none());
    browser.feed(action("client.close"))?;
    assert!(matches!(
        browser.feed(Event::Raw(b"y".to_vec()))?,
        Effect::Mutate(_)
    ));
    assert!(matches!(
        browser.feed(action("session.refresh"))?,
        Effect::Refresh
    ));
    browser.feed(action("client.browser.right"))?;
    assert!(matches!(
        browser.feed(action("client.browser.choose"))?,
        Effect::Attach(_)
    ));
    assert!(matches!(
        browser.feed(action("client.browser.exit"))?,
        Effect::Exit
    ));
    Ok(())
}

#[test]
fn mouse_uses_rendered_offsets_and_respects_modal_boundaries() -> Result<()> {
    use crate::{mouse::Mouse, shortcuts::Event};
    let mut browser = Browser {
        sessions: vec![fixture()],
        ..Default::default()
    };
    let template = browser.sessions[0].windows[0].clone();
    browser.sessions[0].windows = (0..30)
        .map(|n| {
            let mut window = template.clone();
            window.id = format!("window:{n}");
            window.name = format!("tab {n}");
            window
        })
        .collect();
    browser.window = 25;
    browser.preview_hidden = true;
    let mut screen = UiTerminal::new(ratatui::backend::TestBackend::new(80, 10))?;
    let mut hits = vec![];
    screen.draw(|frame| {
        hits = render(
            frame,
            &browser,
            &crate::preview::Frame::default(),
            &Snapshot::default(),
            "",
        );
    })?;
    let press = Mouse::parse(b"\x1b[<0;45;3M").unwrap();
    let hit = hits.iter().find_map(|h| h.at(press)).unwrap();
    assert_eq!(hit.0, Focus::Windows);
    assert!(
        hit.1 > 0 && hit.1 < 25,
        "hit must account for scrolled list offset"
    );
    browser.mouse(press, Some(hit), false)?;
    assert_eq!(browser.window, hit.1);
    assert_eq!(browser.focus, Focus::Windows);
    assert!(matches!(
        browser.mouse(press, Some(hit), true)?,
        Effect::Attach(_)
    ));
    let wheel = Mouse::parse(b"\x1b[<65;45;3M").unwrap();
    browser.mouse(wheel, Some(hit), false)?;
    assert_eq!(browser.window, hit.1 + 1);
    browser.dialog = Some(browser.start_dialog("client.rename")?);
    let selected = browser.window;
    let text = browser.dialog.as_ref().unwrap().text.clone();
    browser.mouse(press, Some((Focus::Windows, 0)), true)?;
    browser.feed(Event::Mouse(wheel))?;
    assert_eq!(browser.window, selected);
    assert_eq!(browser.dialog.as_ref().unwrap().text, text);
    Ok(())
}
