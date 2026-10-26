//! Partages d'écran et webcams : la vidéo (H.264, voir `capture::video`) et
//! le son des partages.
//!
//! **Images** : une par flux QUIC unidirectionnel (fiable, sans bloquer le
//! reste), numérotées et marquées « image clé » quand elles se décodent
//! seules. Les autres dépendent de la précédente : un spectateur à qui il en
//! manque une attend la prochaine image clé.
//!
//! - membre → hôte : `[seq u32][drapeaux u8][h264…]` ;
//! - hôte → spectateur, et d'un participant à l'autre :
//!   `[auteur u32][seq u32][drapeaux u8][h264…]`.
//!
//! Les images de webcam prennent le même chemin, marquées d'un drapeau : elles
//! partent vers tout le salon vocal, pas seulement vers ceux qui regardent.
//!
//! Un spectateur en retard n'accumule rien : on saute ses images jusqu'à la
//! prochaine image clé ([`Lane`]), et il remet dans l'ordre celles qui
//! arrivent ([`Reorder`]).
//!
//! **Son** : datagrammes comme la voix (voir [`super::voice`], dont il
//! partage la numérotation des types), une trame Opus stéréo de 20 ms chacun,
//! envoyés aux seuls spectateurs :
//!
//! - membre → hôte : `[SOUND][seq u16][opus…]` ;
//! - hôte → spectateur, et d'un participant à l'autre :
//!   `[SOUND][auteur u32][seq u16][opus…]`.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use bytes::Bytes;
use quinn::Connection;

use super::protocol::PeerId;

/// Une image plus grosse est refusée.
pub const MAX_IMAGE: usize = 4 * 1024 * 1024;

/// Drapeau : image clé (se décode sans les précédentes).
const KEY: u8 = 1;
/// Drapeau : image de webcam (sinon, d'un partage d'écran).
const CAMERA: u8 = 2;

/// Une image du flux vidéo d'un partage ou d'une webcam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub seq: u32,
    pub key: bool,
    /// De la webcam (sa numérotation est à part).
    pub camera: bool,
    pub data: Bytes,
}

fn header(v: &mut Vec<u8>, frame: &Frame) {
    v.extend_from_slice(&frame.seq.to_be_bytes());
    let mut flags = 0;
    if frame.key {
        flags |= KEY;
    }
    if frame.camera {
        flags |= CAMERA;
    }
    v.push(flags);
}

fn parse(d: &[u8]) -> Option<Frame> {
    let (seq, rest) = d.split_first_chunk::<4>()?;
    let (flags, data) = rest.split_first()?;
    if data.is_empty() {
        return None;
    }
    Some(Frame {
        seq: u32::from_be_bytes(*seq),
        key: flags & KEY != 0,
        camera: flags & CAMERA != 0,
        data: Bytes::copy_from_slice(data),
    })
}

/// Image telle qu'elle part vers l'hôte.
pub fn for_host(frame: &Frame) -> Bytes {
    let mut v = Vec::with_capacity(5 + frame.data.len());
    header(&mut v, frame);
    v.extend_from_slice(&frame.data);
    v.into()
}

pub fn from_member(d: &[u8]) -> Option<Frame> {
    parse(d)
}

/// Image telle qu'elle part vers un spectateur.
pub fn to_member(from: PeerId, frame: &Frame) -> Bytes {
    let mut v = Vec::with_capacity(9 + frame.data.len());
    v.extend_from_slice(&from.to_be_bytes());
    header(&mut v, frame);
    v.extend_from_slice(&frame.data);
    v.into()
}

pub fn from_host(d: &[u8]) -> Option<(PeerId, Frame)> {
    let (from, rest) = d.split_first_chunk::<4>()?;
    Some((PeerId::from_be_bytes(*from), parse(rest)?))
}

/// Envoie une image déjà mise en forme et attend qu'elle soit arrivée.
pub async fn send(conn: &Connection, seq: u32, wire: Bytes) -> Result<()> {
    let mut s = conn.open_uni().await?;
    // Les plus anciennes d'abord : une image clé n'est pas doublée par les
    // suivantes, qui ne servent à rien sans elle.
    let _ = s.set_priority(-((seq & 0x7fff_ffff) as i32));
    s.write_chunk(wire).await?;
    s.finish()?;
    // Arrivée confirmée : c'est ce qui mesure le retard du lien.
    if s.stopped().await?.is_some() {
        bail!("image refusée");
    }
    Ok(())
}

pub async fn recv(stream: &mut quinn::RecvStream) -> Result<Vec<u8>> {
    let data = stream.read_to_end(MAX_IMAGE + 9).await?;
    if data.is_empty() {
        bail!("image vide");
    }
    Ok(data)
}

/// Envoie `frame` (déjà mise en forme : `wire`) sur `lane` si le lien suit.
/// Renvoie vrai s'il faut demander une image clé au partageur.
pub fn offer(conn: &Connection, lane: &Arc<Lane>, frame: &Frame, wire: &Bytes) -> bool {
    let admit = lane.admit(frame.seq, frame.key, Instant::now());
    if admit.send {
        let (conn, lane, wire, seq) = (conn.clone(), lane.clone(), wire.clone(), frame.seq);
        tokio::spawn(async move {
            let ok = send(&conn, seq, wire).await.is_ok();
            lane.done(seq, ok);
        });
    }
    admit.want_key
}

/// Images en route au-delà desquelles le lien est en retard…
const MAX_IN_FLIGHT: usize = 8;
/// … ou quand la plus ancienne n'est toujours pas arrivée après ce délai.
const MAX_LAG: Duration = Duration::from_millis(500);
/// Écart minimal entre deux demandes d'image clé.
pub const ASK_EVERY: Duration = Duration::from_secs(1);

/// Les images envoyées à un destinataire (un spectateur, ou l'hôte qui
/// relaie). Quand son lien prend du retard, ses images sont sautées plutôt
/// qu'empilées ; comme la suivante dépend de celle qui manque, il n'en reçoit
/// plus jusqu'à la prochaine image clé.
#[derive(Default)]
pub struct Lane(Mutex<LaneState>);

#[derive(Default)]
struct LaneState {
    /// Images parties, pas encore arrivées (numéro, départ).
    in_flight: Vec<(u32, Instant)>,
    /// Il a tout reçu depuis la dernière image clé (faux au début : un
    /// nouveau spectateur commence par une image clé).
    synced: bool,
    /// Dernière demande d'image clé faite pour lui.
    asked: Option<Instant>,
}

/// Décision pour une image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Admit {
    pub send: bool,
    /// Demander une image clé au partageur.
    pub want_key: bool,
}

impl Lane {
    pub fn admit(&self, seq: u32, key: bool, now: Instant) -> Admit {
        let mut s = self.0.lock().unwrap();
        let busy = s.in_flight.len() >= MAX_IN_FLIGHT
            || s.in_flight
                .iter()
                .any(|(_, t)| now.saturating_duration_since(*t) > MAX_LAG);
        let send = if busy {
            s.synced = false;
            false
        } else {
            s.synced |= key;
            s.synced
        };
        if send {
            s.in_flight.push((seq, now));
        }
        let want_key = !s.synced && s.asked.is_none_or(|t| now.saturating_duration_since(t) >= ASK_EVERY);
        if want_key {
            s.asked = Some(now);
        }
        Admit { send, want_key }
    }

    /// L'image `seq` est arrivée (ou perdue, si `ok` est faux).
    pub fn done(&self, seq: u32, ok: bool) {
        let mut s = self.0.lock().unwrap();
        s.in_flight.retain(|(n, _)| *n != seq);
        if !ok {
            s.synced = false;
        }
    }

    /// Repart de zéro (nouveau spectateur, ou qui a perdu le fil) : plus
    /// rien ne part avant une image clé. `asked` : elle vient d'être
    /// demandée ; sinon elle le sera à la prochaine image.
    pub fn reset(&self, asked: Option<Instant>) {
        let mut s = self.0.lock().unwrap();
        s.synced = false;
        s.asked = asked;
    }
}

/// Attente maximale d'une image manquante (encore en route) avant de
/// redemander une image clé.
const GAP_WAIT: Duration = Duration::from_millis(700);
/// Images en avance gardées au plus.
const MAX_EARLY: usize = 60;
/// Une image clé aussi loin dans le passé : le partageur a recommencé sa numérotation.
const REWIND: u32 = 600;

/// Côté spectateur : remet les images dans l'ordre, et repère celles qui
/// manquent pour de bon (il faut alors une image clé).
#[derive(Default)]
pub struct Reorder {
    /// Numéro de la prochaine image à décoder ; `None` en attendant une image clé.
    next: Option<u32>,
    /// Arrivées avant leur tour (une précédente est encore en route).
    early: BTreeMap<u32, Frame>,
    /// Depuis quand une image manque.
    gap_since: Option<Instant>,
    asked: Option<Instant>,
}

impl Reorder {
    /// Range une image reçue ; renvoie celles qu'on peut décoder, dans l'ordre.
    pub fn push(&mut self, frame: Frame, now: Instant) -> Vec<Frame> {
        let seq = frame.seq;
        let fresh = |n: u32| seq >= n || n - seq > REWIND;
        match self.next {
            // Image clé : on repart d'elle (ce qui la précède ne sert plus).
            next if frame.key && next.is_none_or(fresh) => {
                self.early.retain(|s, _| *s > seq);
                self.next = Some(seq.wrapping_add(1));
                let mut out = vec![frame];
                self.drain(&mut out, now);
                out
            }
            // En attente d'une image clé, ou déjà passée : inutilisable.
            None => Vec::new(),
            Some(n) if frame.seq < n => Vec::new(),
            Some(n) if frame.seq == n => {
                self.next = Some(n.wrapping_add(1));
                let mut out = vec![frame];
                self.drain(&mut out, now);
                out
            }
            Some(_) => {
                self.early.insert(frame.seq, frame);
                self.gap_since.get_or_insert(now);
                if self.early.len() > MAX_EARLY {
                    self.lost();
                }
                Vec::new()
            }
        }
    }

    fn drain(&mut self, out: &mut Vec<Frame>, now: Instant) {
        while let Some(n) = self.next
            && let Some(f) = self.early.remove(&n)
        {
            out.push(f);
            self.next = Some(n.wrapping_add(1));
        }
        self.gap_since = (!self.early.is_empty()).then_some(now);
    }

    /// Le fil est perdu (image manquante, décodage raté) : on attend une image clé.
    pub fn lost(&mut self) {
        self.next = None;
        self.early.clear();
        self.gap_since = None;
    }

    /// En attente d'une image clé.
    pub fn waiting(&self) -> bool {
        self.next.is_none()
    }

    /// Faut-il demander une image clé ? (au plus une fois par seconde)
    pub fn want_key(&mut self, now: Instant) -> bool {
        if self
            .gap_since
            .is_some_and(|t| now.saturating_duration_since(t) >= GAP_WAIT)
        {
            self.lost();
        }
        let ask = self.waiting() && self.asked.is_none_or(|t| now.saturating_duration_since(t) >= ASK_EVERY);
        if ask {
            self.asked = Some(now);
        }
        ask
    }
}

/// Type de datagramme du son des partages (ceux de la voix vont de 1 à 3).
const SOUND: u8 = 4;

/// Trame de son reçue par l'hôte, ou par un spectateur (`from`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sound<'a> {
    pub from: PeerId,
    pub seq: u16,
    pub opus: &'a [u8],
}

pub fn sound_for_host(seq: u16, opus: &[u8]) -> Bytes {
    let mut v = Vec::with_capacity(3 + opus.len());
    v.push(SOUND);
    v.extend_from_slice(&seq.to_be_bytes());
    v.extend_from_slice(opus);
    v.into()
}

/// Trame envoyée par le membre `from` à l'hôte.
pub fn sound_from_member(from: PeerId, d: &[u8]) -> Option<Sound<'_>> {
    match d {
        [SOUND, s0, s1, opus @ ..] if !opus.is_empty() => Some(Sound {
            from,
            seq: u16::from_be_bytes([*s0, *s1]),
            opus,
        }),
        _ => None,
    }
}

pub fn sound_to_member(sound: Sound) -> Bytes {
    let mut v = Vec::with_capacity(7 + sound.opus.len());
    v.push(SOUND);
    v.extend_from_slice(&sound.from.to_be_bytes());
    v.extend_from_slice(&sound.seq.to_be_bytes());
    v.extend_from_slice(sound.opus);
    v.into()
}

pub fn sound_from_host(d: &[u8]) -> Option<Sound<'_>> {
    match d {
        [SOUND, p0, p1, p2, p3, s0, s1, opus @ ..] if !opus.is_empty() => Some(Sound {
            from: PeerId::from_be_bytes([*p0, *p1, *p2, *p3]),
            seq: u16::from_be_bytes([*s0, *s1]),
            opus,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::voice;

    fn frame(seq: u32, key: bool) -> Frame {
        Frame {
            seq,
            key,
            camera: false,
            data: Bytes::from(vec![seq as u8; 3]),
        }
    }

    #[test]
    fn images_aller_retour() {
        let f = frame(70_000, true);
        assert_eq!(from_member(&for_host(&f)), Some(f.clone()));
        assert_eq!(from_host(&to_member(9, &f)), Some((9, f)));
        let p = frame(3, false);
        assert_eq!(from_host(&to_member(1, &p)).unwrap().1, p);
        // Webcam : le drapeau survit au voyage, image clé ou non.
        for key in [true, false] {
            let c = Frame {
                camera: true,
                ..frame(5, key)
            };
            assert_eq!(from_member(&for_host(&c)), Some(c.clone()));
            assert_eq!(from_host(&to_member(2, &c)), Some((2, c)));
        }
        // Tronquée, ou sans contenu : refusée.
        assert!(from_member(&[0, 0, 0, 1]).is_none());
        assert!(from_member(&[0, 0, 0, 1, KEY]).is_none());
        assert!(from_host(&[0, 0, 0]).is_none());
    }

    #[test]
    fn son_aller_retour_sans_se_confondre_avec_la_voix() {
        let up = sound_for_host(513, b"opus");
        let got = sound_from_member(7, &up).unwrap();
        assert_eq!((got.from, got.seq, got.opus), (7, 513, &b"opus"[..]));
        let down = sound_to_member(got);
        assert_eq!(sound_from_host(&down), Some(got));
        // Ni la voix ne passe pour du son, ni l'inverse.
        assert!(voice::from_member(&up).is_none());
        assert!(voice::from_host(&down).is_none());
        let talk = voice::for_host(1, b"voix", voice::Route::Room);
        assert!(sound_from_member(7, &talk).is_none());
        assert!(sound_from_host(&voice::to_member(7, voice::Frame { seq: 1, opus: b"voix" }, false)).is_none());
        assert!(sound_from_host(&[SOUND, 0, 0, 0, 7, 0, 1]).is_none());
    }

    #[test]
    fn nouveau_spectateur_commence_par_une_image_cle() {
        let lane = Lane::default();
        let t = Instant::now();
        // Image ordinaire : inutile sans la précédente, on demande une image clé.
        assert_eq!(
            lane.admit(1, false, t),
            Admit {
                send: false,
                want_key: true
            }
        );
        // Demande déjà faite : pas de nouvelle avant une seconde.
        assert!(!lane.admit(2, false, t).want_key);
        assert!(lane.admit(3, true, t).send);
        assert!(lane.admit(4, false, t).send);
    }

    #[test]
    fn lien_en_retard_saute_jusqu_a_l_image_cle() {
        let lane = Lane::default();
        let t = Instant::now();
        assert!(lane.admit(0, true, t).send);
        for seq in 1..MAX_IN_FLIGHT as u32 {
            assert!(lane.admit(seq, false, t).send);
        }
        // Trop d'images en route : celle-ci saute, et le spectateur attend une image clé.
        let a = lane.admit(8, false, t);
        assert!(!a.send && a.want_key);
        for seq in 0..8 {
            lane.done(seq, true);
        }
        // Le lien a rattrapé son retard, mais la suite ne se décoderait plus.
        assert!(!lane.admit(9, false, t).send);
        assert!(lane.admit(10, true, t).send);
        assert!(lane.admit(11, false, t).send);
        // Une image qui traîne trop longtemps compte comme du retard.
        let a = lane.admit(12, false, t + ASK_EVERY);
        assert!(!a.send && a.want_key);
        // Une image perdue en route : pareil.
        let lane = Lane::default();
        assert!(lane.admit(0, true, t).send);
        lane.done(0, false);
        assert!(!lane.admit(1, false, t).send);
        // Spectateur qui a perdu le fil : demande immédiate…
        let lane = Lane::default();
        assert!(lane.admit(0, true, t).send);
        lane.reset(None);
        assert!(lane.admit(1, false, t).want_key);
        // … sauf si elle vient d'être faite.
        lane.reset(Some(t));
        assert_eq!(
            lane.admit(2, false, t),
            Admit {
                send: false,
                want_key: false
            }
        );
    }

    fn seqs(v: Vec<Frame>) -> Vec<u32> {
        v.into_iter().map(|f| f.seq).collect()
    }

    #[test]
    fn images_remises_dans_l_ordre() {
        let mut r = Reorder::default();
        let t = Instant::now();
        // Rien ne se décode avant une image clé.
        assert!(r.push(frame(4, false), t).is_empty());
        assert_eq!(seqs(r.push(frame(5, true), t)), [5]);
        // 7 arrive avant 6 : gardée, puis rendue derrière 6.
        assert!(r.push(frame(7, false), t).is_empty());
        assert_eq!(seqs(r.push(frame(6, false), t)), [6, 7]);
        // Doublon ou image déjà passée : ignorée.
        assert!(r.push(frame(6, false), t).is_empty());
        assert!(!r.want_key(t));
        // Une image clé plus récente fait sauter ce qui manque encore.
        assert!(r.push(frame(9, false), t).is_empty());
        assert_eq!(seqs(r.push(frame(10, true), t)), [10]);
        assert_eq!(seqs(r.push(frame(11, false), t)), [11]);
        // Une ancienne image clé arrivée en retard ne fait pas reculer.
        assert!(r.push(frame(8, true), t).is_empty());
        // Le partageur a recommencé à zéro : on le suit.
        assert_eq!(seqs(r.push(frame(2000, true), t)), [2000]);
        assert_eq!(seqs(r.push(frame(0, true), t)), [0]);
    }

    #[test]
    fn trou_qui_dure_demande_une_image_cle() {
        let mut r = Reorder::default();
        let t = Instant::now();
        r.push(frame(0, true), t);
        assert!(r.push(frame(2, false), t).is_empty());
        // L'image 1 est peut-être encore en route : on patiente.
        assert!(!r.want_key(t + GAP_WAIT / 2));
        // Toujours rien : on attend une image clé et on la demande.
        assert!(r.want_key(t + GAP_WAIT));
        assert!(r.waiting());
        // L'image manquante arrive trop tard : plus rien ne passe…
        assert!(r.push(frame(1, false), t + GAP_WAIT).is_empty());
        assert!(r.push(frame(3, false), t + GAP_WAIT).is_empty());
        // … et la demande n'est refaite qu'une fois par seconde.
        assert!(!r.want_key(t + GAP_WAIT + Duration::from_millis(500)));
        assert!(r.want_key(t + GAP_WAIT + ASK_EVERY));
        assert_eq!(seqs(r.push(frame(4, true), t + GAP_WAIT + ASK_EVERY)), [4]);
        assert!(!r.want_key(t + GAP_WAIT + ASK_EVERY * 3));
        // Décodage raté : pareil.
        r.lost();
        assert!(r.want_key(t + GAP_WAIT + ASK_EVERY * 3));
        // Trop d'images en avance : inutile d'attendre davantage.
        let mut r = Reorder::default();
        r.push(frame(0, true), t);
        for seq in 2..MAX_EARLY as u32 + 3 {
            r.push(frame(seq, false), t);
        }
        assert!(r.waiting());
    }
}
