//! In-memory control connection shared by protocol/lifecycle tests.
use crate::protocol::{Control, Io, Socket, next_json};
use anyhow::Result;
use futures_util::SinkExt;
use serde_json::{Value, json};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, protocol::Role},
};

pub async fn socket_pair() -> (Socket, Socket) {
    let (client, server) = tokio::io::duplex(65536);
    let client: Box<dyn Io> = Box::new(client);
    let server: Box<dyn Io> = Box::new(server);
    let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    (client, server)
}

pub async fn control_pair() -> Result<(Control, Socket)> {
    let (client, mut server) = socket_pair().await;
    let handshake = async {
        next_json(&mut server).await?;
        server
            .send(Message::text(
                json!({"type":"welcome","api_revision":11}).to_string(),
            ))
            .await?;
        Ok::<_, anyhow::Error>(())
    };
    let (control, _) = tokio::try_join!(Control::hello(client), handshake)?;
    Ok((control, server))
}
pub async fn reply(server: &mut Socket, request: &Value, payload: Value, fail: bool) -> Result<()> {
    let value = if fail {
        json!({"type":"response","id":request["id"],"error":{"message":format!("{} failed", request["method"])}})
    } else {
        json!({"type":"response","id":request["id"],"payload":payload})
    };
    server.send(Message::text(value.to_string())).await?;
    Ok(())
}
