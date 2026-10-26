//! Son d'un partage d'écran regardé : trames Opus stéréo remises dans l'ordre
//! (tampon de gigue), décodées puis ajoutées au son des haut-parleurs, à son
//! propre volume. Indépendant des voix ([`super::dsp::Mixer`]).

use std::collections::{HashMap, VecDeque};

use opus::{Channels, Decoder};

use super::dsp::{RATE, Resampler};
use crate::core::protocol::PeerId;

/// 20 ms à 48 kHz, par voie.
const FRAME: usize = 960;
/// Avance gardée avant de jouer (le son arrive avant l'image, qui se décode
/// ensuite : un peu d'attente les remet ensemble).
const DEPTH: usize = 4;
/// Au-delà, on saute des trames pour rattraper le retard.
const MAX_DEPTH: usize = 12;
/// Un paquet plus loin que ça dans le futur : on se recale dessus.
const MAX_AHEAD: u16 = 50;

/// Le son du partage regardé.
#[derive(Default)]
pub struct Player {
    /// Celui dont on joue le son (un seul écran regardé à la fois).
    from: Option<PeerId>,
    decoder: Option<Decoder>,
    /// `slots[0]` est la trame `next_seq` ; `None` = pas (encore) reçue.
    slots: VecDeque<Option<Vec<u8>>>,
    next_seq: u16,
    /// Échantillons décodés pas encore joués (gauche, droite).
    pcm: VecDeque<[f32; 2]>,
    playing: bool,
    /// Volume choisi pour le partage de certains participants (1 = normal).
    gains: HashMap<PeerId, f32>,
}

impl Player {
    pub fn set_gain(&mut self, peer: PeerId, gain: f32) {
        self.gains.insert(peer, gain);
    }

    /// Plus rien à jouer (on ne regarde plus).
    pub fn stop(&mut self) {
        self.from = None;
        self.decoder = None;
        self.slots.clear();
        self.pcm.clear();
        self.playing = false;
    }

    fn resync(&mut self, seq: u16) {
        self.slots.clear();
        self.next_seq = seq;
    }

    pub fn push(&mut self, from: PeerId, seq: u16, opus: &[u8]) {
        if self.from != Some(from) {
            self.stop();
            let Ok(decoder) = Decoder::new(RATE, Channels::Stereo) else {
                return;
            };
            self.from = Some(from);
            self.decoder = Some(decoder);
            self.next_seq = seq;
        }
        let mut ahead = seq.wrapping_sub(self.next_seq);
        if ahead >= 0x8000 {
            // Déjà joué : trop tard. Un grand saut en arrière veut dire que la
            // numérotation est repartie de zéro.
            if 0u16.wrapping_sub(ahead) <= MAX_AHEAD {
                return;
            }
            self.resync(seq);
            ahead = 0;
        } else if ahead >= MAX_AHEAD {
            self.resync(seq);
            ahead = 0;
        }
        let at = ahead as usize;
        while self.slots.len() <= at {
            self.slots.push_back(None);
        }
        self.slots[at] = Some(opus.to_vec());
    }

    /// Décode la trame suivante (une trame perdue est comblée par le décodeur).
    fn decode_next(&mut self, pcm: &mut [f32]) -> bool {
        let (Some(slot), Some(decoder)) = (self.slots.pop_front(), &mut self.decoder) else {
            return false;
        };
        self.next_seq = self.next_seq.wrapping_add(1);
        let n = match slot {
            Some(opus) => decoder.decode_float(&opus, pcm, false),
            None => decoder.decode_float(&[], &mut pcm[..2 * FRAME], false),
        };
        if let Ok(n) = n {
            self.pcm.extend(pcm[..2 * n].as_chunks::<2>().0);
        }
        true
    }

    /// Remplit `out` (stéréo, 48 kHz) avec le son du partage, ou du silence.
    pub fn pull(&mut self, out: &mut [[f32; 2]]) {
        out.fill([0.0; 2]);
        let Some(from) = self.from else { return };
        if !self.playing && self.slots.len() + self.pcm.len() / FRAME >= DEPTH {
            self.playing = true;
        }
        if !self.playing {
            return;
        }
        let mut pcm = [0f32; 2 * 5760];
        // Rafale après un trou réseau : on saute plutôt que de garder le retard.
        while self.slots.len() > MAX_DEPTH {
            self.decode_next(&mut pcm);
            self.pcm.clear();
        }
        while self.pcm.len() < out.len() && self.decode_next(&mut pcm) {}
        let gain = self.gains.get(&from).copied().unwrap_or(1.0);
        let n = out.len().min(self.pcm.len());
        for (o, [l, r]) in out.iter_mut().zip(self.pcm.drain(..n)) {
            *o = [l * gain, r * gain];
        }
        if self.pcm.is_empty() && self.slots.is_empty() {
            // Plus rien (silence chez le partageur, ou réseau en retard) : on réattend un peu d'avance.
            self.playing = false;
        }
    }
}

/// Côté haut-parleurs : le son à jouer (gauche, droite), rééchantillonné de
/// 48 kHz à la fréquence de la sortie.
pub struct Stereo {
    resamplers: [Resampler; 2],
    /// Rééchantillonné, pas encore joué (gauche, droite).
    staging: [Vec<f32>; 2],
    chunk: Vec<[f32; 2]>,
    side: Vec<f32>,
    out: Vec<[f32; 2]>,
}

impl Stereo {
    pub fn new(rate: u32) -> Self {
        Self {
            resamplers: [Resampler::new(RATE, rate), Resampler::new(RATE, rate)],
            staging: [Vec::new(), Vec::new()],
            chunk: vec![[0.0; 2]; FRAME / 2],
            side: vec![0.0; FRAME / 2],
            out: Vec::new(),
        }
    }

    /// Les `frames` trames suivantes de la sortie ; `fill` donne le son par
    /// blocs de 10 ms à 48 kHz.
    pub fn pull(&mut self, frames: usize, mut fill: impl FnMut(&mut [[f32; 2]])) -> &[[f32; 2]] {
        while self.staging[0].len() < frames {
            fill(&mut self.chunk);
            for (c, (resampler, staging)) in self.resamplers.iter_mut().zip(&mut self.staging).enumerate() {
                for (s, lr) in self.side.iter_mut().zip(&self.chunk) {
                    *s = lr[c];
                }
                resampler.process(&self.side, staging);
            }
        }
        // Les deux voies avancent au même pas : elles ont la même longueur.
        let n = frames.min(self.staging[1].len());
        let [left, right] = &mut self.staging;
        self.out.clear();
        self.out
            .extend(left.drain(..n).zip(right.drain(..n)).map(|(l, r)| [l, r]));
        self.out.resize(frames, [0.0; 2]);
        &self.out
    }
}

/// Ce que reçoit la voie `channel` d'une sortie à `channels` voies : gauche
/// et droite, les deux mêlées sur une sortie mono, rien sur les autres.
pub fn side(lr: [f32; 2], channel: usize, channels: usize) -> f32 {
    match (channels, channel) {
        (1, _) => (lr[0] + lr[1]) / 2.0,
        (_, 0 | 1) => lr[channel],
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::audio::dsp::level_db;
    use opus::{Application, Bitrate, Encoder};

    /// Trames Opus stéréo : 440 Hz à gauche, silence à droite.
    fn frames(n: usize) -> Vec<Vec<u8>> {
        let mut enc = Encoder::new(RATE, Channels::Stereo, Application::Audio).unwrap();
        enc.set_bitrate(Bitrate::Bits(128_000)).unwrap();
        let mut out = [0u8; 1500];
        (0..n)
            .map(|k| {
                let pcm: Vec<f32> = (0..FRAME)
                    .flat_map(|i| {
                        let t = (k * FRAME + i) as f32 / RATE as f32;
                        [0.5 * (t * 440.0 * std::f32::consts::TAU).sin(), 0.0]
                    })
                    .collect();
                let len = enc.encode_float(&pcm, &mut out).unwrap();
                out[..len].to_vec()
            })
            .collect()
    }

    fn levels(out: &[[f32; 2]]) -> (f32, f32) {
        let l: Vec<f32> = out.iter().map(|s| s[0]).collect();
        let r: Vec<f32> = out.iter().map(|s| s[1]).collect();
        (level_db(&l), level_db(&r))
    }

    #[test]
    fn stereo_dans_l_ordre_au_volume_choisi() {
        let mut p = Player::default();
        let mut out = vec![[0.0; 2]; FRAME];
        // Rien avant d'avoir un peu d'avance.
        let f = frames(12);
        p.push(3, 100, &f[0]);
        p.pull(&mut out);
        assert!(out.iter().all(|s| *s == [0.0; 2]));
        // Dans le désordre : remis dans l'ordre.
        for (i, opus) in f.iter().enumerate().skip(1).rev() {
            p.push(3, 100 + i as u16, opus);
        }
        p.pull(&mut out);
        p.pull(&mut out);
        let (l, r) = levels(&out);
        assert!(l > -15.0 && r < l - 20.0, "gauche {l:.1} dB, droite {r:.1} dB");
        // Volume à moitié : 6 dB de moins.
        p.set_gain(3, 0.5);
        p.pull(&mut out);
        let (half, _) = levels(&out);
        assert!((l - half - 6.0).abs() < 2.0, "{l:.1} → {half:.1}");
        // On ne regarde plus : silence.
        p.stop();
        p.pull(&mut out);
        assert!(out.iter().all(|s| *s == [0.0; 2]));
    }

    #[test]
    fn autre_partage_on_repart_de_zero() {
        let mut p = Player::default();
        let f = frames(6);
        for (i, opus) in f.iter().enumerate() {
            p.push(1, i as u16, opus);
        }
        // L'écran regardé change : l'ancien son ne se mélange pas au nouveau.
        p.push(2, 9000, &f[0]);
        assert_eq!((p.from, p.slots.len(), p.next_seq), (Some(2), 1, 9000));
    }

    #[test]
    fn sortie_a_la_frequence_de_la_carte_son() {
        let player = Arc::new(Mutex::new(Player::default()));
        for (i, opus) in frames(20).iter().enumerate() {
            player.lock().unwrap().push(1, i as u16, opus);
        }
        let mut output = Stereo::new(44_100);
        let mut got = Vec::new();
        for _ in 0..10 {
            got.extend_from_slice(output.pull(441, |out| player.lock().unwrap().pull(out)));
        }
        assert_eq!(got.len(), 4410);
        let (l, _) = levels(&got[2000..]);
        assert!(l > -15.0, "{l:.1} dB");
        // Sur une sortie mono, gauche et droite se mélangent ; au-delà de deux voies, rien.
        assert!((side([0.4, 0.2], 0, 1) - 0.3).abs() < 1e-6);
        assert_eq!((side([0.4, 0.2], 1, 2), side([0.4, 0.2], 4, 6)), (0.2, 0.0));
    }
}
