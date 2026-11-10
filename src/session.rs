//! Explicit asynchronous session cleanup and terminal stream initialization.
use crate::{domain::Target, protocol, stream, terminal};
use anyhow::Result;
use serde_json::{Value, json};
pub async fn resize(
    target: &Target,
    control: &mut protocol::Control,
    size: (u16, u16),
) -> Result<()> {
    control.call("com.superlogical.terminal.resize",json!({"session_id":target.session,"block_id":target.block,"args":{"columns":size.0,"rows":size.1}})).await?;
    Ok(())
}

/// Attach for one operation and always await detach, including operation failure.
/// Cancellation drops this future: callers must then drop its dedicated connection.
pub async fn with_session<T>(
    control: &mut protocol::Control,
    session: &str,
    operation: impl AsyncFnOnce(&mut protocol::Control, Value) -> Result<T>,
) -> Result<T> {
    let attached = control
        .call("session.attach", json!({"session_id":session}))
        .await?;
    let result = operation(control, attached).await;
    let cleanup = control
        .call("session.detach", json!({"session_id":session}))
        .await;
    match (result, cleanup) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error.context("releasing session attachment")),
        (Err(error), Err(cleanup)) => {
            Err(error.context(format!("session cleanup also failed: {cleanup:#}")))
        }
    }
}

pub async fn open_terminal(
    control: &mut protocol::Control,
    socket: &str,
    target: &Target,
) -> Result<(protocol::Socket, terminal::State)> {
    let ticket = control.call("data.connect", json!({"session_id":target.session,"block_id":target.block,"data_ref_id":"terminal","subprotocol":stream::PROTOCOL})).await?;
    let ticket: crate::wire::Ticket = crate::wire::decode(&ticket, "data.connect response")?;
    let mut data = protocol::socket(socket, "/api/data", stream::PROTOCOL).await?;
    let welcome = stream::handshake(&mut data, &ticket.ticket).await?;
    Ok((data, terminal::State::new(welcome.nonce)))
}

/// Continue consuming and drawing terminal output while an ordered RPC is pending.
/// Input is not retried or reordered; the outer loop resumes reading it afterward.
pub async fn while_streaming<T>(
    operation: impl std::future::Future<Output = Result<T>>,
    data: &mut protocol::Socket,
    state: &mut terminal::State,
    mut draw: impl FnMut(&mut terminal::State) -> Result<()>,
) -> Result<T> {
    tokio::pin!(operation);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(16));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = false;
    let mut output_error = None;
    loop {
        tokio::select! {
            result = &mut operation => return result.and_then(|value| match output_error {
                Some(error) => Err(error), None => Ok(value),
            }),
            bytes = stream::next_binary(data), if output_error.is_none() => {
                output_error = bytes.and_then(|bytes| state.feed(&bytes)).err();
                dirty = true;
            }
            _ = tick.tick(), if dirty && state.is_ready() && output_error.is_none() => {
                match state.render_held(std::time::Instant::now()) {
                    Ok(true) => {},
                    Ok(false) => { output_error = draw(state).err(); dirty = false; },
                    Err(error) => output_error = Some(error),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::next_json,
        test_support::{control_pair, reply},
    };
    #[tokio::test]
    async fn operation_and_cleanup_failures_are_both_preserved() -> Result<()> {
        for (operation_fails, detach_fails) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let (mut control, mut server) = control_pair().await?;
            let serve = async {
                for (method, fail) in [
                    ("session.attach", false),
                    ("test.work", operation_fails),
                    ("session.detach", detach_fails),
                ] {
                    let request = next_json(&mut server).await?;
                    assert_eq!(request["method"], method);
                    reply(&mut server, &request, json!({}), fail).await?;
                }
                Ok::<_, anyhow::Error>(())
            };
            let client = async {
                let result = with_session(&mut control, "s", async |control, _| {
                    control.call("test.work", json!({})).await
                })
                .await;
                if operation_fails || detach_fails {
                    let error = format!("{:#}", result.unwrap_err());
                    if operation_fails {
                        assert!(error.contains("test.work"));
                    }
                    if detach_fails {
                        assert!(error.contains("session.detach"));
                    }
                } else {
                    assert!(result.is_ok());
                }
                Ok::<_, anyhow::Error>(())
            };
            tokio::try_join!(serve, client)?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn failed_attach_does_not_execute_operation() -> Result<()> {
        let (mut control, mut server) = control_pair().await?;
        let serve = async {
            let request = next_json(&mut server).await?;
            reply(&mut server, &request, json!({}), true).await
        };
        let client = async {
            let result = with_session(&mut control, "s", async |_, _| -> Result<()> {
                panic!("operation after failed attach")
            })
            .await;
            assert!(result.is_err());
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(serve, client)?;
        Ok(())
    }
    #[tokio::test]
    async fn cancelling_pending_attach_closes_dedicated_connection() -> Result<()> {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut control, mut server) = control_pair().await?;
                let task = crate::task::Task::spawn(async move {
                    let _ = with_session(&mut control, "s", async |_, _| Ok(())).await;
                });
                let request = next_json(&mut server).await?;
                assert_eq!(request["method"], "session.attach");
                task.stop().await;
                let closed =
                    tokio::time::timeout(std::time::Duration::from_secs(1), next_json(&mut server))
                        .await?;
                assert!(closed.is_err());
                Ok::<_, anyhow::Error>(())
            })
            .await
    }
    #[tokio::test]
    async fn delayed_rpc_does_not_freeze_terminal_output() -> Result<()> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        let (mut client, mut server) = crate::test_support::socket_pair().await;
        let mut terminal = ghostty_vt::Terminal::new(ghostty_vt::TerminalOptions {
            cols: 20,
            rows: 2,
            ..Default::default()
        })?;
        terminal.vt_write(b"output during RPC");
        let bytes = ghostty_vt::snapshot::encode(&terminal)?.to_vec();
        server.send(Message::Binary(bytes.into())).await?;
        let (complete, result) = tokio::sync::oneshot::channel();
        let mut complete = Some(complete);
        let mut state = terminal::State::new(42);
        let operation = async {
            result.await?;
            Ok(())
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            while_streaming(operation, &mut client, &mut state, |state| {
                let text = terminal::frame(state.terminal_mut()?, 20, 2, false)?;
                assert!(text.contains("output during RPC"));
                complete
                    .take()
                    .expect("draw before completion")
                    .send(())
                    .unwrap();
                Ok(())
            }),
        )
        .await??;
        Ok(())
    }
    #[tokio::test]
    async fn output_failure_does_not_abandon_the_pending_rpc() -> Result<()> {
        let (mut data, server) = crate::test_support::socket_pair().await;
        drop(server);
        let (complete, result) = tokio::sync::oneshot::channel();
        let operation = async {
            result.await?;
            Ok(())
        };
        let mut state = terminal::State::new(42);
        let pending = while_streaming(operation, &mut data, &mut state, |_| Ok(()));
        tokio::pin!(pending);
        tokio::select! {
            biased;
            _ = &mut pending => panic!("output failure abandoned the RPC"),
            _ = tokio::task::yield_now() => {},
        }
        complete.send(()).unwrap();
        assert!(pending.await.is_err());
        Ok(())
    }
}
