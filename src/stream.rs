use crate::protocol::Socket;
use crate::vt::{Terminal, snapshot};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
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

/// Frames the documented GHOSTSNP envelope. Record CRCs and contents are
/// validated transactionally by Ghostty once FINISH is present.
#[derive(Default)]
pub struct SnapshotBuffer {
    wire: Vec<u8>,
    phase: Boundary,
}
#[derive(Default)]
enum Boundary {
    #[default]
    Envelope,
    Record(usize),
    Body {
        end: usize,
        finish: bool,
    },
}
impl SnapshotBuffer {
    pub fn push(&mut self, incoming: &[u8]) -> Result<Option<(Terminal, Vec<u8>)>> {
        const MAX: usize = 256 * 1024 * 1024;
        ensure!(
            incoming.len() <= MAX.saturating_sub(self.wire.len()),
            "snapshot exceeds 256 MiB"
        );
        self.wire.extend_from_slice(incoming);
        loop {
            match self.phase {
                Boundary::Envelope => {
                    let Some(envelope) = self.wire.get(..10) else {
                        return Ok(None);
                    };
                    ensure!(
                        &envelope[..8] == b"GHOSTSNP"
                            && u16::from_le_bytes(envelope[8..].try_into()?) == 1,
                        "unsupported Ghostty snapshot envelope"
                    );
                    self.phase = Boundary::Record(10);
                }
                Boundary::Record(offset) => {
                    let Some(header) = self.wire.get(offset..offset + 10) else {
                        return Ok(None);
                    };
                    let payload = u32::from_le_bytes(header[2..6].try_into()?) as usize;
                    let end = offset
                        .checked_add(10)
                        .and_then(|n| n.checked_add(payload))
                        .filter(|n| *n <= MAX)
                        .ok_or_else(|| anyhow::anyhow!("snapshot record exceeds limit"))?;
                    let tag = u16::from_le_bytes(header[..2].try_into()?);
                    self.phase = Boundary::Body {
                        end,
                        finish: tag == 6,
                    };
                }
                Boundary::Body { end, finish } => {
                    if self.wire.len() < end {
                        return Ok(None);
                    }
                    if !finish {
                        self.phase = Boundary::Record(end);
                        continue;
                    }
                    let mut terminal =
                        snapshot::decode(&self.wire[..end], crate::graphics::IMAGE_LIMIT)
                            .context("snapshot decoding")?;
                    crate::graphics::configure(&mut terminal)
                        .context("configuring restored graphics")?;
                    let following = self.wire.split_off(end);
                    self.wire.clear();
                    self.phase = Boundary::Envelope;
                    return Ok(Some((terminal, following)));
                }
            }
        }
    }
}
