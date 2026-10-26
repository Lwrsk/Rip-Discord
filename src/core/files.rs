//! Pièces jointes : chaque transfert a son propre flux QUIC bidirectionnel
//! (ouvert par le membre vers l'hôte), pour ne jamais bloquer le texte ni la
//! voix pendant qu'un gros fichier passe.
//!
//! - envoi : `[FileRequest::Put][contenu…]` → `[1 octet : accepté ?]` ;
//! - téléchargement : `[FileRequest::Get]` → `[1 octet : trouvé ?][contenu…]`.
//!
//! Le contenu est désigné par son empreinte SHA-256 : l'hôte comme le membre
//! la vérifient, personne ne peut faire passer un fichier pour un autre.

use anyhow::{Result, bail, ensure};
use quinn::{Connection, RecvStream, SendStream};
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

use super::{
    card::MAX_FILE_BYTES,
    framing::{read_msg, write_msg},
};

pub type FileHash = [u8; 32];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileRequest {
    Put { hash: FileHash, size: u64 },
    Get { hash: FileHash },
}

pub fn hash(bytes: &[u8]) -> FileHash {
    digest(&SHA256, bytes).as_ref().try_into().unwrap()
}

/// Envoie un fichier à l'hôte (qui le garde pour les autres).
pub async fn put(conn: &Connection, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() as u64 <= MAX_FILE_BYTES, "fichier trop gros");
    let (mut send, mut recv) = conn.open_bi().await?;
    let request = FileRequest::Put {
        hash: hash(bytes),
        size: bytes.len() as u64,
    };
    write_msg(&mut send, &request).await?;
    send.write_all(bytes).await?;
    send.finish()?;
    let mut ok = [0u8];
    recv.read_exact(&mut ok).await?;
    ensure!(ok[0] == 1, "l'hôte a refusé le fichier");
    Ok(())
}

/// Télécharge un fichier depuis l'hôte (il le cherche chez les autres
/// membres s'il ne l'a pas).
pub async fn get(conn: &Connection, wanted: FileHash) -> Result<Vec<u8>> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, &FileRequest::Get { hash: wanted }).await?;
    send.finish()?;
    let mut found = [0u8];
    recv.read_exact(&mut found).await?;
    if found[0] != 1 {
        bail!("fichier introuvable : personne de connecté ne l'a");
    }
    let bytes = recv.read_to_end(MAX_FILE_BYTES as usize).await?;
    ensure!(hash(&bytes) == wanted, "fichier abîmé en route");
    Ok(bytes)
}

/// Côté hôte : lit la demande d'un flux entrant.
pub async fn read_request(recv: &mut RecvStream) -> Result<FileRequest> {
    match read_msg(recv).await? {
        Some(r) => Ok(r),
        None => bail!("demande de fichier vide"),
    }
}

/// Côté hôte : lit le contenu d'un envoi et vérifie qu'il correspond.
pub async fn read_put(recv: &mut RecvStream, wanted: FileHash, size: u64) -> Result<Vec<u8>> {
    ensure!(size <= MAX_FILE_BYTES, "fichier trop gros");
    let bytes = recv.read_to_end(size as usize).await?;
    ensure!(
        bytes.len() as u64 == size && hash(&bytes) == wanted,
        "fichier abîmé en route"
    );
    Ok(bytes)
}

/// Côté hôte : répond à un envoi.
pub async fn answer_put(send: &mut SendStream, ok: bool) -> Result<()> {
    send.write_all(&[ok as u8]).await?;
    send.finish()?;
    Ok(())
}

/// Côté hôte : répond à un téléchargement.
pub async fn answer_get(send: &mut SendStream, bytes: Option<&[u8]>) -> Result<()> {
    send.write_all(&[bytes.is_some() as u8]).await?;
    if let Some(b) = bytes {
        send.write_all(b).await?;
    }
    send.finish()?;
    // Attend que tout soit parti avant de lâcher le flux.
    let _ = send.stopped().await;
    Ok(())
}
