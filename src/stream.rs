use crate::protocol::Socket;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use ghostty_vt::{Terminal, snapshot};
use tokio_tungstenite::tungstenite::Message;

pub const PROTOCOL: &str = "rex.data.terminal.v1";

#[derive(Debug)]
pub struct HandshakeRejected(pub u8);
impl std::fmt::Display for HandshakeRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rex rejected data handshake (reason {})", self.0)
    }
}
impl std::error::Error for HandshakeRejected {}

pub struct Welcome {
    pub nonce: u64,
}

pub async fn handshake(socket: &mut Socket, ticket: &str) -> Result<Welcome> {
    let ticket = URL_SAFE_NO_PAD.decode(ticket)?;
    ensure!(ticket.len() == 16, "invalid data ticket length");
    let mut message = vec![1];
    message.extend(ticket);
    exchange(socket, message).await
}

async fn exchange(socket: &mut Socket, message: Vec<u8>) -> Result<Welcome> {
    socket.send(Message::Binary(message.into())).await?;
    let bytes =
        tokio::time::timeout(std::time::Duration::from_secs(8), next_binary(socket)).await??;
    if bytes.first() == Some(&0x82) {
        return Err(HandshakeRejected(*bytes.get(1).unwrap_or(&0)).into());
    }
    ensure!(
        bytes.len() == 32 && bytes[0] == 0x81,
        "invalid data WELCOME"
    );
    let nonce = u64::from_le_bytes(bytes[24..32].try_into()?);
    ensure!(nonce != 0, "invalid channel nonce");
    Ok(Welcome { nonce })
}

#[derive(Debug)]
pub struct DataClosed {
    pub code: Option<u16>,
    pub reason: String,
}
impl std::fmt::Display for DataClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "data connection closed ({:?}): {}",
            self.code, self.reason
        )
    }
}
impl std::error::Error for DataClosed {}

pub async fn next_binary(socket: &mut Socket) -> Result<Vec<u8>> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Binary(bytes))) => return Ok(bytes.to_vec()),
            Some(Ok(Message::Close(frame))) => {
                return Err(DataClosed {
                    code: frame.as_ref().map(|f| f.code.into()),
                    reason: frame.map(|f| f.reason.to_string()).unwrap_or_default(),
                }
                .into());
            }
            Some(Ok(_)) => socket.flush().await?,
            Some(Err(error)) => return Err(error.into()),
            None => bail!("data connection ended"),
        }
    }
}

/// Locate FINISH without guessing based on WebSocket frame boundaries. Ghostty
/// validates record order, payloads and CRCs once the complete snapshot is here.
#[derive(Default)]
pub struct SnapshotBuffer {
    bytes: Vec<u8>,
    scan: usize,
}
impl SnapshotBuffer {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<(Terminal, Vec<u8>)>> {
        const LIMIT: usize = 256 * 1024 * 1024;
        ensure!(
            self.bytes.len() + bytes.len() <= LIMIT,
            "snapshot exceeds 256 MiB"
        );
        self.bytes.extend_from_slice(bytes);
        if self.scan == 0 {
            if self.bytes.len() < 10 {
                return Ok(None);
            }
            ensure!(
                &self.bytes[..10] == b"GHOSTSNP\x01\x00",
                "unsupported Ghostty snapshot envelope"
            );
            self.scan = 10;
        }
        while self.bytes.len() >= self.scan + 10 {
            let header = &self.bytes[self.scan..self.scan + 10];
            let tag = u16::from_le_bytes(header[..2].try_into()?);
            let len = u32::from_le_bytes(header[2..6].try_into()?) as usize;
            ensure!(
                len <= LIMIT - self.scan - 10,
                "snapshot record exceeds limit"
            );
            let end = self.scan + 10 + len;
            if end > self.bytes.len() {
                return Ok(None);
            }
            self.scan = end;
            if tag == 6 {
                // We already buffer through FINISH, so the published complete
                // decoder suffices; no patched incremental API is needed.
                let mut source = std::io::Cursor::new(&self.bytes[..end]);
                let terminal = snapshot::Decoder::new(&mut source)?
                    .decode()
                    .context("snapshot decoding")?;
                ensure!(
                    source.position() == end as u64,
                    "snapshot boundary mismatch"
                );
                let tail = self.bytes[end..].to_vec();
                self.bytes.clear();
                return Ok(Some((terminal, tail)));
            }
        }
        Ok(None)
    }
}
