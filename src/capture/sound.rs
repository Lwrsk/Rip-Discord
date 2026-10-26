//! Son du partage d'écran : ce que joue la fenêtre partagée (et les
//! programmes qu'elle a lancés), ou pour un écran entier tout le son du PC
//! sauf le nôtre — les voix des autres, jouées par l'appli, ne repartent pas
//! vers eux.
//!
//! Sous Windows (10 version 2004 ou plus récent), par la capture « par
//! processus » de WASAPI. Ailleurs, ou si elle manque, l'image part sans le
//! son : jamais la capture de tout le son, qui renverrait les voix en écho.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use opus::{Application, Bitrate, Channels, Encoder};
use tokio::sync::mpsc;

use crate::audio::dsp::SHARED;

/// Fréquence du son capturé (Opus), en stéréo.
pub const RATE: u32 = 48_000;
/// 20 ms à 48 kHz, par voie.
pub const FRAME: usize = 960;
/// Débit Opus : de la musique ou un jeu, pas une voix.
const BITRATE: i32 = 128_000;
/// Après autant de trames de silence (1 s), plus rien ne part.
const HANGOVER: u32 = 50;
/// En dessous (dB), c'est du silence.
const SILENCE_DB: f32 = -80.0;

/// La capture du son est possible sur ce système.
pub const SUPPORTED: bool = cfg!(windows);

/// Ce qu'on capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Ce processus et ses enfants (une fenêtre partagée).
    App(u32),
    /// Tout sauf nous (un écran entier).
    AllButMe,
}

/// Son capturé (stéréo entrelacé, 48 kHz) → trames Opus de 20 ms.
pub struct Packer {
    encoder: Encoder,
    pending: Vec<f32>,
    /// Trames encore envoyées après la dernière qui n'était pas du silence.
    open_for: u32,
}

impl Packer {
    pub fn new() -> Result<Self> {
        let mut encoder = Encoder::new(RATE, Channels::Stereo, Application::Audio)?;
        encoder.set_bitrate(Bitrate::Bits(BITRATE))?;
        Ok(Self {
            encoder,
            pending: Vec::with_capacity(8 * FRAME),
            open_for: 0,
        })
    }

    /// Ajoute des échantillons ; renvoie les trames prêtes à partir.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<u8>> {
        self.pending.extend_from_slice(samples);
        let mut frames = Vec::new();
        let mut out = [0u8; 1500];
        while self.pending.len() >= 2 * FRAME {
            let frame: Vec<f32> = self.pending.drain(..2 * FRAME).collect();
            let energy = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
            if 10.0 * energy.max(1e-12).log10() >= SILENCE_DB {
                self.open_for = HANGOVER;
            } else {
                self.open_for = self.open_for.saturating_sub(1);
            }
            if self.open_for == 0 {
                continue;
            }
            if let Ok(n) = self.encoder.encode_float(&frame, &mut out) {
                frames.push(out[..n].to_vec());
            }
        }
        frames
    }
}

/// Capture en cours ; s'arrête quand elle est lâchée.
pub struct Capture {
    stop: Arc<AtomicBool>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        SHARED.clear();
    }
}

impl Capture {
    /// Capture `target` ; les trames Opus arrivent dans `tx`. Une erreur si
    /// le système ne le permet pas.
    pub fn start(target: Target, tx: mpsc::Sender<Vec<u8>>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        imp::start(target, tx, stop.clone())?;
        Ok(Self { stop })
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;

    pub fn start(_: Target, _: mpsc::Sender<Vec<u8>>, _: Arc<AtomicBool>) -> Result<()> {
        anyhow::bail!("le son des applications ne se partage que sous Windows")
    }
}

#[cfg(windows)]
mod imp {
    use std::{sync::mpsc as std_mpsc, time::Duration};

    use anyhow::{Context, anyhow};
    use tracing::warn;
    use wasapi::{AudioCaptureClient, AudioClient, Direction, Handle, SampleType, StreamMode, WaveFormat};

    use super::*;

    /// Octets par trame d'échantillons : deux voies en f32.
    const BLOCK: usize = 8;

    pub fn start(target: Target, tx: mpsc::Sender<Vec<u8>>, stop: Arc<AtomicBool>) -> Result<()> {
        let (ready_tx, ready) = std_mpsc::channel();
        // Les objets audio de Windows ne changent pas de fil : tout se passe ici.
        std::thread::Builder::new().name("son-partage".into()).spawn(move || {
            let _ = wasapi::initialize_mta();
            let opened = open(target).and_then(|o| Ok((o, Packer::new()?)));
            let ((client, capture, event), mut packer) = match opened {
                Ok(o) => {
                    let _ = ready_tx.send(Ok(()));
                    o
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let mut buf = Vec::new();
            let mut samples = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                // Rien ne joue : pas d'événement, on revérifie juste l'arrêt.
                if event.wait_for_event(200).is_err() {
                    continue;
                }
                samples.clear();
                if let Err(e) = read(&capture, &mut buf, &mut samples) {
                    warn!("son du partage arrêté : {e:#}");
                    break;
                }
                // Ça sort aussi de nos haut-parleurs : le micro doit l'ignorer.
                SHARED.push_stereo(&samples);
                for opus in packer.push(&samples) {
                    if let Err(mpsc::error::TrySendError::Closed(_)) = tx.try_send(opus) {
                        return;
                    }
                }
            }
            let _ = client.stop_stream();
        })?;
        ready
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("la capture du son ne répond pas"))?
    }

    fn open(target: Target) -> Result<(AudioClient, AudioCaptureClient, Handle)> {
        let (pid, include) = match target {
            Target::App(pid) => (pid, true),
            // Sans l'arbre de notre processus : nos haut-parleurs (les voix) n'y sont pas.
            Target::AllButMe => (std::process::id(), false),
        };
        let mut client = AudioClient::new_application_loopback_client(pid, include)
            .context("capture par processus indisponible (Windows 10 version 2004 ou plus récent requis)")?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, 2, None);
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: 200_000,
        };
        client.initialize_client(&format, &Direction::Capture, &mode)?;
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        Ok((client, capture, event))
    }

    /// Lit tout ce qui attend, en échantillons stéréo entrelacés.
    fn read(capture: &AudioCaptureClient, buf: &mut Vec<u8>, samples: &mut Vec<f32>) -> Result<()> {
        loop {
            let frames = capture.get_next_packet_size()?.unwrap_or(0) as usize;
            if frames == 0 {
                return Ok(());
            }
            buf.resize(frames * BLOCK, 0);
            let (got, info) = capture.read_from_device(buf)?;
            let bytes = &buf[..got as usize * BLOCK];
            if info.flags.silent {
                samples.resize(samples.len() + got as usize * 2, 0.0);
            } else {
                samples.extend(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let t = i as f32 / RATE as f32;
                [
                    0.4 * (t * 440.0 * std::f32::consts::TAU).sin(),
                    0.4 * (t * 660.0 * std::f32::consts::TAU).sin(),
                ]
            })
            .collect()
    }

    #[test]
    fn trames_de_20_ms_et_silence_coupe() {
        let mut p = Packer::new().unwrap();
        // 100 ms de musique, par petits morceaux comme la carte son les livre.
        let music = stereo(5 * FRAME);
        let frames: Vec<_> = music.chunks(441 * 2).flat_map(|c| p.push(c)).collect();
        assert_eq!(frames.len(), 5);
        assert!(frames.iter().all(|f| !f.is_empty() && f.len() < 1200));
        // Le silence part encore un moment (fin d'un son), puis plus rien.
        let quiet = p.push(&vec![0.0; 2 * FRAME * (HANGOVER as usize + 10)]);
        assert_eq!(quiet.len(), HANGOVER as usize - 1);
        assert!(p.push(&vec![0.0; 2 * FRAME * 5]).is_empty());
    }

    #[cfg(not(windows))]
    #[test]
    fn pas_de_son_hors_de_windows() {
        let (tx, _rx) = mpsc::channel(4);
        assert!(Capture::start(Target::AllButMe, tx).is_err());
        const { assert!(!SUPPORTED) };
    }
}
