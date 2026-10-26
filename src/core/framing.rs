//! Découpage des paquets sur un flux QUIC : longueur u32 big-endian + postcard.

use anyhow::{Context, Result, bail};
use quinn::{RecvStream, SendStream};
use serde::{Serialize, de::DeserializeOwned};

use super::protocol::Packet;

/// Taille maximale d'un paquet de contrôle/texte.
pub const MAX_FRAME: usize = 64 * 1024;

pub async fn write_packet(send: &mut SendStream, packet: &Packet) -> Result<()> {
    write_msg(send, packet).await
}

/// Renvoie `None` quand le pair a fermé le flux proprement.
pub async fn read_packet(recv: &mut RecvStream) -> Result<Option<Packet>> {
    read_msg(recv).await
}

/// Écrit n'importe quel message sérialisable (même découpage que les paquets).
pub async fn write_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<()> {
    let bytes = postcard::to_stdvec(msg)?;
    if bytes.len() > MAX_FRAME {
        bail!("paquet trop gros ({} octets)", bytes.len());
    }
    send.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    send.write_all(&bytes).await?;
    Ok(())
}

pub async fn read_msg<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match recv.read_exact(&mut len).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        bail!("trame annoncée trop grosse ({len} octets)");
    }
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await?;
    let packet = postcard::from_bytes(&buf).context("paquet illisible")?;
    Ok(Some(packet))
}
