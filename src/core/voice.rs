//! Paquets de voix : datagrammes QUIC (non fiables, sans attente), un par
//! trame Opus de 20 ms.
//!
//! La voix circule d'abord directement entre participants (voir
//! `client::mesh`, même format que « hôte → membres ») ; l'hôte du serveur ne
//! relaie que pour ceux qui ne peuvent pas se joindre directement.
//!
//! - membre → hôte : `[VOICE][seq u16][opus…]` (tout le salon),
//!   `[RELAY][seq u16][n u8][destinataire u32 × n][opus…]` (seulement ceux-là,
//!   faute de connexion directe), ou `[WHISPER][…même chose…]` pour chuchoter ;
//! - hôte → membres, et d'un participant à l'autre :
//!   `[VOICE|WHISPER][auteur u32][seq u16][opus…]`.
//!
//! Les destinataires sont dans chaque paquet : relâcher la touche de
//! chuchotement ne peut pas faire partir une trame au mauvais public.

use bytes::Bytes;

use super::protocol::PeerId;

const VOICE: u8 = 1;
const WHISPER: u8 = 2;
const RELAY: u8 = 3;
/// Un chuchotement vise au plus ce nombre de personnes.
pub const MAX_TARGETS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub seq: u16,
    pub opus: &'a [u8],
}

/// Trame reçue par l'hôte : pour tout le salon, ou seulement pour `targets`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uplink<'a> {
    pub frame: Frame<'a>,
    pub targets: Option<Vec<PeerId>>,
    pub whisper: bool,
}

/// À qui une trame envoyée à l'hôte est destinée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route<'a> {
    Room,
    /// Ces participants seulement (pas de connexion directe avec eux).
    Relay(&'a [PeerId]),
    /// Chuchotement à ces participants.
    Whisper(&'a [PeerId]),
}

/// Trame reçue par un membre.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Downlink<'a> {
    pub from: PeerId,
    pub frame: Frame<'a>,
    /// Chuchotée à quelques-uns (dont moi) plutôt que dite à tout le salon.
    pub whisper: bool,
}

pub fn for_host(seq: u16, opus: &[u8], route: Route) -> Bytes {
    let (kind, targets) = match route {
        Route::Room => (VOICE, None),
        Route::Relay(t) => (RELAY, Some(t)),
        Route::Whisper(t) => (WHISPER, Some(t)),
    };
    let targets = targets.map(|t| &t[..t.len().min(MAX_TARGETS)]);
    let mut v = Vec::with_capacity(4 + 4 * targets.map_or(0, |t| t.len()) + opus.len());
    match targets {
        None => {
            v.push(VOICE);
            v.extend_from_slice(&seq.to_be_bytes());
        }
        Some(t) => {
            v.push(kind);
            v.extend_from_slice(&seq.to_be_bytes());
            v.push(t.len() as u8);
            for id in t {
                v.extend_from_slice(&id.to_be_bytes());
            }
        }
    }
    v.extend_from_slice(opus);
    v.into()
}

pub fn from_member(d: &[u8]) -> Option<Uplink<'_>> {
    let (kind, rest) = d.split_first()?;
    let (seq, rest) = rest.split_first_chunk::<2>()?;
    let seq = u16::from_be_bytes(*seq);
    let (targets, opus) = match *kind {
        VOICE => (None, rest),
        WHISPER | RELAY => {
            let (n, rest) = rest.split_first()?;
            let n = *n as usize;
            if n > MAX_TARGETS || rest.len() < 4 * n {
                return None;
            }
            let (ids, opus) = rest.split_at(4 * n);
            let ids = ids
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| PeerId::from_be_bytes(*c))
                .collect();
            (Some(ids), opus)
        }
        _ => return None,
    };
    if opus.is_empty() {
        return None;
    }
    Some(Uplink {
        frame: Frame { seq, opus },
        targets,
        whisper: *kind == WHISPER,
    })
}

pub fn to_member(from: PeerId, frame: Frame, whisper: bool) -> Bytes {
    let mut v = Vec::with_capacity(7 + frame.opus.len());
    v.push(if whisper { WHISPER } else { VOICE });
    v.extend_from_slice(&from.to_be_bytes());
    v.extend_from_slice(&frame.seq.to_be_bytes());
    v.extend_from_slice(frame.opus);
    v.into()
}

pub fn from_host(d: &[u8]) -> Option<Downlink<'_>> {
    match d {
        [kind @ (VOICE | WHISPER), p0, p1, p2, p3, s0, s1, opus @ ..] if !opus.is_empty() => Some(Downlink {
            from: PeerId::from_be_bytes([*p0, *p1, *p2, *p3]),
            frame: Frame {
                seq: u16::from_be_bytes([*s0, *s1]),
                opus,
            },
            whisper: *kind == WHISPER,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aller_retour() {
        let up = for_host(513, b"opus", Route::Room);
        let got = from_member(&up).unwrap();
        let frame = Frame {
            seq: 513,
            opus: b"opus",
        };
        assert_eq!(
            got,
            Uplink {
                frame,
                targets: None,
                whisper: false
            }
        );
        let up = for_host(1, b"x", Route::Relay(&[4]));
        let relayed = from_member(&up).unwrap();
        assert_eq!((relayed.targets, relayed.whisper), (Some(vec![4]), false));
        let down = to_member(7, frame, false);
        assert_eq!(
            from_host(&down),
            Some(Downlink {
                from: 7,
                frame,
                whisper: false
            })
        );
        assert!(from_member(&[1, 0]).is_none());
    }

    #[test]
    fn chuchotement() {
        let up = for_host(9, b"psst", Route::Whisper(&[2, 3]));
        let got = from_member(&up).unwrap();
        assert_eq!(got.targets, Some(vec![2, 3]));
        assert!(got.whisper);
        assert_eq!(got.frame.opus, b"psst");
        let down = to_member(1, got.frame, true);
        assert!(from_host(&down).unwrap().whisper);
        // Liste tronquée ou annoncée trop longue : refusé.
        assert!(from_member(&[WHISPER, 0, 9, 2, 0, 0, 0, 2]).is_none());
        assert!(from_member(&[WHISPER, 0, 9, 200, 1]).is_none());
    }
}
