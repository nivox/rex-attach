use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, client::IntoClientRequest},
};

pub trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}
pub type Socket = WebSocketStream<Box<dyn Io>>;

pub async fn socket(path: &str, route: &str, protocol: &str) -> Result<Socket> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connecting to {path}"))?;
        let mut request = format!("ws://localhost{route}").into_client_request()?;
        request
            .headers_mut()
            .insert("Sec-WebSocket-Protocol", protocol.parse()?);
        let stream: Box<dyn Io> = Box::new(stream);
        let (socket, response) = tokio_tungstenite::client_async(request, stream).await?;
        ensure!(
            response
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|s| s.to_str().ok())
                == Some(protocol),
            "server did not negotiate {protocol}"
        );
        Ok(socket)
    })
    .await
    .context("connection timed out")?
}

pub async fn next_json(socket: &mut Socket) -> Result<Value> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text)?),
            Some(Ok(Message::Ping(_))) => socket.flush().await?,
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(other)) => bail!("unexpected control message: {other:?}"),
            Some(Err(e)) => return Err(e.into()),
            None => bail!("control connection closed"),
        }
    }
}

pub struct Control {
    socket: Socket,
    id: u64,
    events: Option<std::collections::VecDeque<Value>>,
    response_seq: u64,
}
impl Control {
    /// Read idle events only when no RPC is in flight. There is one socket reader.
    pub async fn next_event(&mut self) -> Result<Value> {
        next_json(&mut self.socket).await
    }
    pub async fn resolve_session(&mut self, session: &str) -> Result<String> {
        ensure!(!session.trim().is_empty(), "--session must not be empty");
        let reply = self
            .call(
                "target.resolve",
                json!({"kinds":["session"],"queries":[session]}),
            )
            .await?;
        let target = resolved_target(&reply, "session", session)?;
        let id = target
            .session_id
            .as_deref()
            .context("resolved session has no ID")?;
        ensure!(
            id.starts_with("session:"),
            "resolver returned an invalid session ID"
        );
        Ok(id.to_owned())
    }
    pub async fn resolve_targets(
        &mut self,
        session: &str,
        block: &str,
    ) -> Result<(String, String)> {
        ensure!(!block.trim().is_empty(), "--block must not be empty");
        let session_id = self.resolve_session(session).await?;
        let reply = self
            .call(
                "target.resolve",
                json!({"kinds":["block"],"queries":[block],"scope":{"session":session_id}}),
            )
            .await?;
        let target = resolved_target(&reply, "block", block)?;
        ensure!(
            target.session_id.as_deref() == Some(session_id.as_str()),
            "resolved block belongs to a different session"
        );
        let block_id = target
            .block_id
            .as_deref()
            .context("resolved block has no ID")?
            .to_owned();
        ensure!(
            block_id.starts_with("block:"),
            "resolver returned an invalid block ID"
        );
        Ok((session_id, block_id))
    }
    pub async fn connect(path: &str) -> Result<Self> {
        Self::hello(socket(path, "/api/control", "rex.control.v1").await?).await
    }
    pub(crate) async fn hello(mut socket: Socket) -> Result<Self> {
        socket.send(Message::text(json!({"type":"hello", "api_revision":11, "api_min_revision":10,
            "client":{"kind":"cli","name":"rex-attach","version":env!("CARGO_PKG_VERSION"),"platform":std::env::consts::OS}}).to_string())).await?;
        let welcome =
            tokio::time::timeout(Duration::from_secs(10), next_json(&mut socket)).await??;
        ensure!(
            welcome["type"] == "welcome",
            "control handshake rejected: {welcome}"
        );
        ensure!(
            welcome["api_revision"]
                .as_u64()
                .is_some_and(|r| (10..=11).contains(&r)),
            "unsupported negotiated API revision: {welcome}"
        );
        Ok(Self {
            socket,
            id: 0,
            events: None,
            response_seq: 0,
        })
    }
    pub fn capture_events(&mut self) {
        self.events = Some(Default::default());
    }
    pub fn take_events(&mut self) -> Vec<Value> {
        self.events
            .as_mut()
            .map(|events| events.drain(..).collect())
            .unwrap_or_default()
    }
    pub fn response_seq(&self) -> u64 {
        self.response_seq
    }
    pub async fn call(&mut self, method: &str, payload: Value) -> Result<Value> {
        self.id += 1;
        let id = self.id.to_string();
        self.socket
            .send(Message::text(
                json!({"type":"request","id":id,"method":method,"payload":payload}).to_string(),
            ))
            .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let value = next_json(&mut self.socket).await?;
                // Match the outstanding RPC while retaining subscribed session events.
                if value["type"] == "response" && value["id"] == id {
                    self.response_seq = value["seq"].as_u64().unwrap_or(0);
                    ensure!(
                        value.get("error").is_none_or(Value::is_null),
                        "{method}: {}",
                        value["error"]
                    );
                    return Ok(value["payload"].clone());
                }
                if value["type"] == "event"
                    && let Some(events) = &mut self.events
                {
                    // Bounded buffering for subscribers doing RPCs. They also
                    // periodically re-query snapshots to recover from overflow.
                    if value.to_string().len() <= 16 * 1024 {
                        if events.len() >= 1024 {
                            events.pop_front();
                        }
                        events.push_back(value);
                    }
                }
            }
        })
        .await
        .with_context(|| format!("{method} timed out"))?
    }
}

fn resolved_target(reply: &Value, kind: &str, query: &str) -> Result<crate::wire::ResolvedTarget> {
    use crate::wire::{Resolution, ResolveReply};
    let mut reply: ResolveReply = crate::wire::decode(reply, "target.resolve response")?;
    ensure!(
        reply.results.len() == 1,
        "expected one target.resolve result"
    );
    match reply.results.pop().expect("one resolution") {
        Resolution::Resolved { target } => {
            ensure!(
                target.kind == kind,
                "resolver returned an unexpected target kind"
            );
            Ok(target)
        }
        Resolution::Ambiguous { candidates } => {
            let choices = candidates
                .iter()
                .map(|target| {
                    let id = if kind == "session" {
                        &target.session_id
                    } else {
                        &target.block_id
                    };
                    format!(
                        "{} ({})",
                        target.label.as_deref().unwrap_or(""),
                        id.as_deref().unwrap_or("unknown ID")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!("ambiguous {kind} {query:?}; use a full ID. Candidates: {choices}")
        }
        Resolution::NotFound { reason } => bail!(
            "{kind} {query:?} not found: {}",
            reason.as_deref().unwrap_or("no matching target")
        ),
        Resolution::Unknown => bail!("{kind} {query:?} not found: no matching target"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vt::{Terminal, TerminalOptions, snapshot};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn pair() -> (Socket, Socket) {
        let (a, b) = tokio::io::duplex(65536);
        let a: Box<dyn Io> = Box::new(a);
        let b: Box<dyn Io> = Box::new(b);
        (
            WebSocketStream::from_raw_socket(a, Role::Client, None).await,
            WebSocketStream::from_raw_socket(b, Role::Server, None).await,
        )
    }

    // Exercise the actual client protocol and snapshot pipeline without requiring
    // an OS socket, Rex account, or access to a user's running blocks.
    #[tokio::test]
    async fn attach_ticket_restore_input_and_detach() -> Result<()> {
        let (client_control, mut server_control) = pair().await;
        let (mut client_data, mut server_data) = pair().await;
        let ticket = URL_SAFE_NO_PAD.encode([7u8; 16]);
        let server = async {
            let hello = next_json(&mut server_control).await?;
            ensure!(hello["type"] == "hello" && hello["client"]["name"] == "rex-attach");
            server_control
                .send(Message::text(
                    json!({"type":"welcome","api_revision":11}).to_string(),
                ))
                .await?;
            for (kind, query, target) in [
                (
                    "session",
                    "ripple-view",
                    json!({"kind":"session","session_id":"session:test"}),
                ),
                (
                    "block",
                    "68585c",
                    json!({"kind":"block","session_id":"session:test","block_id":"block:test"}),
                ),
            ] {
                let request = next_json(&mut server_control).await?;
                ensure!(request["method"] == "target.resolve");
                ensure!(request["payload"]["kinds"] == json!([kind]));
                ensure!(request["payload"]["queries"] == json!([query]));
                if kind == "block" {
                    ensure!(request["payload"]["scope"]["session"] == "session:test");
                } else {
                    ensure!(request["payload"].get("scope").is_none());
                }
                server_control.send(Message::text(json!({"type":"response","id":request["id"],"payload":{"results":[{"query":query,"status":"resolved","target":target}]}}).to_string())).await?;
            }
            for method in [
                "session.attach",
                "data.connect",
                "com.superlogical.terminal.write",
                "session.detach",
            ] {
                let request = next_json(&mut server_control).await?;
                ensure!(request["method"] == method);
                ensure!(request["payload"]["session_id"] == "session:test");
                if method == "com.superlogical.terminal.write" {
                    ensure!(request["payload"]["block_id"] == "block:test");
                    ensure!(request["payload"]["args"]["data"] == "hello\r");
                }
                // An interleaved event must not be mistaken for an RPC response.
                server_control
                    .send(Message::text(
                        json!({"type":"event","seq":1,"payload":{}}).to_string(),
                    ))
                    .await?;
                server_control.send(Message::text(json!({"type":"response","id":request["id"],"payload":if method == "data.connect" {json!({"ticket":ticket})} else {json!({})}}).to_string())).await?;
                if method == "data.connect" {
                    let hello = crate::stream::next_binary(&mut server_data).await?;
                    ensure!(hello == [vec![1], vec![7u8; 16]].concat());
                    let mut welcome = vec![0u8; 32];
                    welcome[0] = 0x81;
                    welcome[24..32].copy_from_slice(&42u64.to_le_bytes());
                    server_data.send(Message::Binary(welcome.into())).await?;
                    let mut terminal = Terminal::new(TerminalOptions {
                        cols: 20,
                        rows: 3,
                        ..Default::default()
                    })?;
                    terminal.vt_write(b"existing screen\r\n");
                    let mut bytes = snapshot::encode(&terminal)?.to_vec();
                    bytes.extend_from_slice(b"live output");
                    for chunk in bytes.chunks(113) {
                        server_data
                            .send(Message::Binary(chunk.to_vec().into()))
                            .await?;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let client = async {
            let mut control = Control::hello(client_control).await?;
            let (session, block) = control.resolve_targets("ripple-view", "68585c").await?;
            ensure!(session == "session:test" && block == "block:test");
            control
                .call("session.attach", json!({"session_id":"session:test"}))
                .await?;
            let response = control.call("data.connect",json!({"session_id":"session:test","block_id":"block:test","data_ref_id":"terminal","subprotocol":crate::stream::PROTOCOL})).await?;
            let welcome =
                crate::stream::handshake(&mut client_data, response["ticket"].as_str().unwrap())
                    .await?;
            let mut state = crate::terminal::State::new(welcome.nonce);
            loop {
                state.feed(&crate::stream::next_binary(&mut client_data).await?)?;
                if let Ok(terminal) = state.terminal_mut() {
                    let text = crate::terminal::frame(terminal, 20, 3, false)?;
                    if text.contains("live output") {
                        ensure!(text.contains("existing screen"));
                        break;
                    }
                }
            }
            control.call("com.superlogical.terminal.write",json!({"session_id":"session:test","block_id":"block:test","args":{"data":"hello\r"}})).await?;
            control
                .call("session.detach", json!({"session_id":"session:test"}))
                .await?;
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::try_join!(server, client)
        })
        .await??;
        Ok(())
    }

    #[test]
    fn resolution_errors_are_actionable_and_never_pick_a_candidate() {
        let ambiguous = json!({"results":[{"status":"ambiguous","candidates":[
            {"kind":"block","label":"shell","block_id":"block:one"},
            {"kind":"block","label":"shell","block_id":"block:two"}
        ]}]});
        let error = resolved_target(&ambiguous, "block", "shell")
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous block"));
        assert!(error.contains("block:one") && error.contains("block:two"));
        let missing = json!({"results":[{"status":"not_found","reason":"no such session"}]});
        assert!(
            resolved_target(&missing, "session", "missing")
                .unwrap_err()
                .to_string()
                .contains("no such session")
        );
        let wrong_kind = json!({"results":[{"status":"resolved","target":{"kind":"window"}}]});
        assert!(resolved_target(&wrong_kind, "block", "shell").is_err());
    }
}
