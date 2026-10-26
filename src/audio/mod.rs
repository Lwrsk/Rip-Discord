//! Son des salons vocaux : micro et haut-parleurs (cpal), codec Opus.
//!
//! Les flux des cartes son vivent sur un fil à part ([`Engine`]) ; le reste
//! de l'app lui passe la voix reçue et récupère les trames du micro. Les sons
//! d'interface ([`Cue`]) se jouent dans un salon comme en dehors.

pub mod cue;
pub mod dsp;
pub mod share;

pub use cue::Cue;

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc as std_mpsc,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use cpal::{
    FromSample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use tokio::sync::mpsc;
use tracing::warn;

use crate::core::protocol::PeerId;
use dsp::{Capture, Clean, Gate, Limiter, Mixer, Played};

/// Réglages audio de l'utilisateur (gardés sur sa machine).
#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    /// Micro choisi par son nom ; `None` = celui du système.
    pub input: Option<String>,
    pub output: Option<String>,
    /// Sensibilité du micro : en dessous de ce niveau (dB), rien ne part.
    pub threshold_db: f32,
    pub volume: f32,
    /// Appui-pour-parler : le micro ne part que touche enfoncée.
    pub push_to_talk: bool,
    /// Nettoyage du micro : bruit de fond, écho des haut-parleurs, gain.
    pub denoise: bool,
    pub echo_cancel: bool,
    pub auto_gain: bool,
    /// Sons d'interface ([`Cue`]) et leur volume.
    pub cues: bool,
    pub cues_volume: f32,
    /// Webcam choisie par son nom ; `None` = la première trouvée.
    pub camera: Option<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            input: None,
            output: None,
            threshold_db: -50.0,
            volume: 1.0,
            push_to_talk: false,
            denoise: true,
            echo_cancel: true,
            auto_gain: false,
            cues: true,
            cues_volume: 0.6,
            camera: None,
        }
    }
}

impl Prefs {
    pub fn clean(&self) -> Clean {
        Clean {
            noise: self.denoise,
            echo: self.echo_cancel,
            gain: self.auto_gain,
        }
    }
}

pub fn input_devices() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|d| d.map(|d| d.to_string()).collect())
        .unwrap_or_default()
}

pub fn output_devices() -> Vec<String> {
    cpal::default_host()
        .output_devices()
        .map(|d| d.map(|d| d.to_string()).collect())
        .unwrap_or_default()
}

/// En dessous du seuil de détection de la voix, à partir de combien de dB le
/// niveau du micro vaut la peine d'être suivi de près à l'écran.
const NEAR_DB: f32 = 12.0;

/// Ce que l'interface affiche : niveau du micro, qui parle, problèmes.
#[derive(Default)]
pub struct Status {
    level_db: AtomicU32,
    muted: AtomicBool,
    deafened: AtomicBool,
    threshold_db: AtomicU32,
    push_to_talk: AtomicBool,
    /// Touche d'appui-pour-parler ou de chuchotement enfoncée.
    held: AtomicBool,
    /// Je chuchote (à quelques-uns seulement).
    whispering: AtomicBool,
    /// Nettoyage du micro, changé à la volée.
    denoise: AtomicBool,
    echo_cancel: AtomicBool,
    auto_gain: AtomicBool,
    heard: Mutex<HashMap<PeerId, Instant>>,
    /// Ceux qui viennent de me chuchoter quelque chose.
    whispers: Mutex<HashMap<PeerId, Instant>>,
    sent: Mutex<Option<Instant>>,
    pub problems: Mutex<Vec<String>>,
    /// Prévient l'interface quand quelqu'un se met à parler : entre deux,
    /// elle n'a pas à se redessiner sans cesse pour le guetter.
    waker: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl Status {
    fn new(prefs: &Prefs) -> Self {
        let s = Self::default();
        s.level_db.store((-90f32).to_bits(), Ordering::Relaxed);
        s.threshold_db.store(prefs.threshold_db.to_bits(), Ordering::Relaxed);
        s.push_to_talk.store(prefs.push_to_talk, Ordering::Relaxed);
        s.set_clean(prefs.clean());
        s
    }

    fn set_clean(&self, c: Clean) {
        self.denoise.store(c.noise, Ordering::Relaxed);
        self.echo_cancel.store(c.echo, Ordering::Relaxed);
        self.auto_gain.store(c.gain, Ordering::Relaxed);
    }

    fn clean(&self) -> Clean {
        Clean {
            noise: self.denoise.load(Ordering::Relaxed),
            echo: self.echo_cancel.load(Ordering::Relaxed),
            gain: self.auto_gain.load(Ordering::Relaxed),
        }
    }

    pub fn push_to_talk(&self) -> bool {
        self.push_to_talk.load(Ordering::Relaxed)
    }

    pub fn whispering(&self) -> bool {
        self.whispering.load(Ordering::Relaxed)
    }

    /// `peer` me chuchote quelque chose en ce moment.
    pub fn whispers_to_me(&self, peer: PeerId) -> bool {
        self.whispers
            .lock()
            .unwrap()
            .get(&peer)
            .is_some_and(|t| t.elapsed() < Duration::from_millis(400))
    }

    /// Ce qui décide, à cet instant, si le micro part.
    fn gate(&self) -> Gate {
        if self.muted() || self.deafened() {
            Gate::Closed
        } else if self.held.load(Ordering::Relaxed) {
            Gate::Open
        } else if self.push_to_talk() {
            Gate::Closed
        } else {
            Gate::Voice(f32::from_bits(self.threshold_db.load(Ordering::Relaxed)))
        }
    }

    pub fn level_db(&self) -> f32 {
        f32::from_bits(self.level_db.load(Ordering::Relaxed))
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn deafened(&self) -> bool {
        self.deafened.load(Ordering::Relaxed)
    }

    /// `wake` sera appelé quand quelqu'un se met à parler (moi compris), ou
    /// que mon micro approche du seuil.
    pub fn on_activity(&self, wake: impl Fn() + Send + Sync + 'static) {
        let _ = self.waker.set(Box::new(wake));
    }

    fn wake(&self) {
        if let Some(w) = self.waker.get() {
            w();
        }
    }

    /// Il se passe quelque chose à afficher : quelqu'un parle, ou mon micro
    /// entend assez fort pour qu'on suive son niveau.
    pub fn active(&self) -> bool {
        let recent = |t: &Instant| t.elapsed() < Duration::from_millis(600);
        self.level_db() > f32::from_bits(self.threshold_db.load(Ordering::Relaxed)) - NEAR_DB
            || self.sent.lock().unwrap().as_ref().is_some_and(recent)
            || self.heard.lock().unwrap().values().any(recent)
    }

    /// `peer` a parlé il y a moins de 300 ms.
    pub fn speaking(&self, peer: PeerId) -> bool {
        self.heard
            .lock()
            .unwrap()
            .get(&peer)
            .is_some_and(|t| t.elapsed() < Duration::from_millis(300))
    }

    pub fn me_speaking(&self) -> bool {
        self.sent
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < Duration::from_millis(300))
    }
}

/// Trames Opus du micro, numérotées, prêtes à partir.
pub type Frames = mpsc::UnboundedReceiver<(u16, Vec<u8>)>;

/// Micro et haut-parleurs ouverts tant que l'`Engine` vit.
pub struct Engine {
    pub status: Arc<Status>,
    mixer: Arc<Mutex<Mixer>>,
    /// Sortie qui joue les sons d'interface.
    cues: u64,
    /// Son du partage d'écran regardé.
    share: Arc<Mutex<share::Player>>,
    _stop: std_mpsc::Sender<()>,
}

impl Engine {
    /// Ouvre les cartes son. Les trames du micro arrivent dans le récepteur
    /// renvoyé. Un micro ou des haut-parleurs absents ne sont pas une erreur :
    /// ils sont signalés dans [`Status::problems`].
    pub fn start(prefs: &Prefs) -> Result<(Self, Frames)> {
        let status = Arc::new(Status::new(prefs));
        let mut mixer = Mixer::new();
        mixer.volume = prefs.volume;
        let mixer = Arc::new(Mutex::new(mixer));
        let share = Arc::new(Mutex::new(share::Player::default()));
        let (frames_tx, frames_rx) = mpsc::unbounded_channel();
        let (stop, stopped) = std_mpsc::channel::<()>();
        let (ready_tx, ready) = std_mpsc::channel();
        let sink = cue::Sink::new();
        let cues = sink.id();
        // Ce que jouent les haut-parleurs, pour retirer son écho du micro.
        let played = Arc::new(Played::default());

        let (p, st, mx, sh, pl) = (
            prefs.clone(),
            status.clone(),
            mixer.clone(),
            share.clone(),
            played.clone(),
        );
        std::thread::Builder::new().name("audio".into()).spawn(move || {
            // Les flux cpal ne changent pas de fil : ils vivent ici. Un flux
            // qui tombe (casque débranché, sortie par défaut changée dans
            // Windows) ou un périphérique absent est rouvert dès que possible.
            let sink = Arc::new(sink);
            let (in_broken, out_broken) = (Broken::default(), Broken::default());
            let open_in = || open_input(&p, st.clone(), frames_tx.clone(), pl.clone(), &in_broken);
            let open_out = || {
                let fill = room_output(mx.clone(), sh.clone(), st.clone(), played.clone(), sink.clone());
                open_output(p.output.as_deref(), fill, &out_broken)
            };
            let mut input = open_in();
            let mut output = open_out();
            report(&st, &p, &input, &output);
            let _ = ready_tx.send(());
            while let Err(std_mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(REOPEN_EVERY) {
                let mut changed = false;
                if needs_reopen(&in_broken, &input, p.input.as_deref(), Direction::Input) {
                    drop(std::mem::replace(&mut input, Err(anyhow!("fermé"))));
                    input = open_in();
                    changed = true;
                }
                if needs_reopen(&out_broken, &output, p.output.as_deref(), Direction::Output) {
                    drop(std::mem::replace(&mut output, Err(anyhow!("fermé"))));
                    output = open_out();
                    changed = true;
                }
                if changed {
                    report(&st, &p, &input, &output);
                }
            }
            drop((input, output));
        })?;
        ready
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("la carte son ne répond pas"))?;
        Ok((
            Self {
                status,
                mixer,
                cues,
                share,
                _stop: stop,
            },
            frames_rx,
        ))
    }

    pub fn play(&self, from: PeerId, seq: u16, opus: &[u8], whisper: bool) {
        let now = Instant::now();
        let before = self.status.heard.lock().unwrap().insert(from, now);
        if before.is_none_or(|t| now - t > Duration::from_millis(300)) {
            self.status.wake();
        }
        if whisper {
            self.status.whispers.lock().unwrap().insert(from, now);
        }
        if !self.status.deafened() {
            self.mixer.lock().unwrap().push(from, seq, opus);
        }
    }

    pub fn set_muted(&self, muted: bool) {
        self.status.muted.store(muted, Ordering::Relaxed);
    }

    /// Plus rien n'est joué (et le micro est coupé aussi, comme sur Discord).
    pub fn set_deafened(&self, deafened: bool) {
        self.status.deafened.store(deafened, Ordering::Relaxed);
        if deafened {
            self.mixer.lock().unwrap().retain(&[]);
            self.share.lock().unwrap().stop();
        }
    }

    /// Son du partage d'écran regardé (trame Opus stéréo).
    pub fn play_share(&self, from: PeerId, seq: u16, opus: &[u8]) {
        if !self.status.deafened() {
            self.share.lock().unwrap().push(from, seq, opus);
        }
    }

    /// Volume du son du partage de `peer` (1 = normal, 0 = coupé).
    pub fn set_share_volume(&self, peer: PeerId, volume: f32) {
        self.share.lock().unwrap().set_gain(peer, volume);
    }

    /// On ne regarde plus : plus de son de partage.
    pub fn stop_share(&self) {
        self.share.lock().unwrap().stop();
    }

    pub fn set_threshold(&self, db: f32) {
        self.status.threshold_db.store(db.to_bits(), Ordering::Relaxed);
    }

    pub fn set_push_to_talk(&self, on: bool) {
        self.status.push_to_talk.store(on, Ordering::Relaxed);
    }

    /// Suppression du bruit, annulation d'écho, gain automatique : appliqués
    /// à la trame suivante.
    pub fn set_clean(&self, clean: Clean) {
        self.status.set_clean(clean);
    }

    /// Touche d'appui-pour-parler ou de chuchotement enfoncée (`whisper` :
    /// pour quelques-uns seulement).
    pub fn set_held(&self, held: bool, whisper: bool) {
        self.status.held.store(held, Ordering::Relaxed);
        self.status.whispering.store(held && whisper, Ordering::Relaxed);
    }

    pub fn set_volume(&self, volume: f32) {
        self.mixer.lock().unwrap().volume = volume;
    }

    /// Volume d'un participant en particulier (1 = normal).
    pub fn set_peer_volume(&self, peer: PeerId, volume: f32) {
        self.mixer.lock().unwrap().set_gain(peer, volume);
    }

    pub fn retain(&self, members: &[PeerId]) {
        self.mixer.lock().unwrap().retain(members);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Les flux se ferment un peu plus tard, sur leur fil : les sons
        // d'interface n'attendent pas (« au revoir » en quittant le salon).
        cue::retire(self.cues);
    }
}

/// Délai entre deux essais de réouverture d'un micro ou d'une sortie.
const REOPEN_EVERY: Duration = Duration::from_secs(2);

/// Un flux ouvert, et s'il remplace le périphérique choisi (débranché) par
/// celui du système en attendant son retour.
type Opened = Result<(Stream, bool)>;

#[derive(Clone, Copy)]
enum Direction {
    Input,
    Output,
}

/// Levé par la carte son quand un flux ne joue plus et doit être rouvert.
#[derive(Default)]
pub(super) struct Broken(Arc<AtomicBool>);

impl Broken {
    pub(super) fn on_error(&self, what: &'static str) -> impl FnMut(cpal::Error) + Send + 'static {
        let flag = self.0.clone();
        move |e| {
            // Un trou dans le son ou une priorité refusée : le flux continue.
            if matches!(
                e.kind(),
                cpal::ErrorKind::Xrun | cpal::ErrorKind::RealtimeDenied | cpal::ErrorKind::DeviceChanged
            ) {
                return;
            }
            if !flag.swap(true, Ordering::AcqRel) {
                warn!("{what} : {e}, réouverture");
            }
        }
    }

    pub(super) fn take(&self) -> bool {
        self.0.swap(false, Ordering::AcqRel)
    }
}

/// Faut-il rouvrir ce flux : tombé, jamais ouvert, ou le périphérique
/// choisi est de retour ?
fn needs_reopen(broken: &Broken, opened: &Opened, chosen: Option<&str>, dir: Direction) -> bool {
    let host = cpal::default_host();
    broken.take()
        || match opened {
            Err(_) => true,
            Ok((_, false)) => false,
            Ok((_, true)) => chosen.is_some_and(|name| match dir {
                Direction::Input => find(host.input_devices().map(|d| d.into_iter()), name).is_some(),
                Direction::Output => find(host.output_devices().map(|d| d.into_iter()), name).is_some(),
            }),
        }
}

/// Les problèmes de son à montrer, notés dans le journal quand ils changent.
fn report(status: &Status, prefs: &Prefs, input: &Opened, output: &Opened) {
    let mut problems = Vec::new();
    match input {
        Err(_) => problems
            .push("Aucun micro utilisable : branche-en un, ou choisis-le dans tes réglages (onglet Voix).".into()),
        Ok((_, true)) => problems.push(format!(
            "Micro « {} » introuvable : celui du système le remplace en attendant.",
            prefs.input.as_deref().unwrap_or_default()
        )),
        Ok(_) => {}
    }
    match output {
        Err(_) => problems.push(
            "Aucune sortie audio utilisable : branche un casque, ou choisis la sortie dans tes réglages (onglet Voix)."
                .into(),
        ),
        Ok((_, true)) => problems.push(format!(
            "Sortie « {} » introuvable : celle du système la remplace en attendant.",
            prefs.output.as_deref().unwrap_or_default()
        )),
        Ok(_) => {}
    }
    let mut shown = status.problems.lock().unwrap();
    if *shown != problems {
        for (what, r) in [("micro", input), ("sortie audio", output)] {
            match r {
                Err(e) => warn!("{what} : {e:#}"),
                Ok((_, true)) => warn!("{what} : périphérique choisi absent, celui du système à la place"),
                Ok(_) => {}
            }
        }
        if problems.is_empty() && !shown.is_empty() {
            tracing::info!("son rétabli");
        }
        *shown = problems;
    }
}

fn find(devices: Result<impl Iterator<Item = cpal::Device>, cpal::Error>, name: &str) -> Option<cpal::Device> {
    devices.ok()?.find(|d| d.to_string() == name)
}

fn open_input(
    prefs: &Prefs,
    status: Arc<Status>,
    frames: mpsc::UnboundedSender<(u16, Vec<u8>)>,
    played: Arc<Played>,
    broken: &Broken,
) -> Opened {
    let host = cpal::default_host();
    let chosen = prefs
        .input
        .as_deref()
        .and_then(|name| find(host.input_devices().map(|d| d.into_iter()), name));
    let fallback = prefs.input.is_some() && chosen.is_none();
    let device = match chosen {
        Some(d) => d,
        None => host.default_input_device().context("aucun micro trouvé")?,
    };
    let supported = device.default_input_config()?;
    let config: StreamConfig = supported.config();
    let channels = config.channels as usize;
    let mut capture = Capture::new(config.sample_rate)?;
    let mut mono = Vec::new();
    let mut far = Vec::with_capacity(dsp::MAX_PLAYED);
    let mut was_near = false;
    let mut on_samples = move |samples: &mut dyn Iterator<Item = f32>| {
        mono.clear();
        let mut acc = 0.0;
        for (i, s) in samples.enumerate() {
            acc += s;
            if i % channels == channels - 1 {
                mono.push(acc / channels as f32);
                acc = 0.0;
            }
        }
        // Ce qui est sorti des haut-parleurs d'abord : son écho est peut-être
        // déjà dans ce que le micro vient d'entendre.
        capture.set_clean(status.clean());
        played.take(&mut far);
        dsp::SHARED.mix_into(&mut far);
        capture.played(&far);
        let (out, level) = capture.push(&mono, status.gate());
        if let Some(db) = level {
            status.level_db.store(db.to_bits(), Ordering::Relaxed);
            let near = db > f32::from_bits(status.threshold_db.load(Ordering::Relaxed)) - NEAR_DB;
            if near && !was_near {
                status.wake();
            }
            was_near = near;
        }
        if !out.is_empty() {
            let mut sent = status.sent.lock().unwrap();
            if sent.is_none_or(|t| t.elapsed() > Duration::from_millis(300)) {
                status.wake();
            }
            *sent = Some(Instant::now());
        }
        for f in out {
            let _ = frames.send(f);
        }
    };
    let err = broken.on_error("micro");
    let stream = match supported.sample_format() {
        SampleFormat::F32 => {
            device.build_input_stream::<f32, _, _>(config, move |d, _| on_samples(&mut d.iter().copied()), err, None)
        }
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |d, _| on_samples(&mut d.iter().map(|s| <f32 as FromSample<_>>::from_sample_(*s))),
            err,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream::<u16, _, _>(
            config,
            move |d, _| on_samples(&mut d.iter().map(|s| <f32 as FromSample<_>>::from_sample_(*s))),
            err,
            None,
        ),
        other => return Err(anyhow!("format {other} non géré")),
    }?;
    stream.play()?;
    Ok((stream, fallback))
}

/// Ce que joue la sortie d'un salon : les voix et les sons d'interface au
/// milieu, le son du partage regardé en stéréo (rien si son coupé), le tout
/// passé au limiteur. Ramené en mono, il sert aussi de référence à
/// l'annulation d'écho : sinon le son d'un jeu regardé repartirait par le micro.
fn room_output(
    mixer: Arc<Mutex<Mixer>>,
    share: Arc<Mutex<share::Player>>,
    status: Arc<Status>,
    played: Arc<Played>,
    cues: Arc<cue::Sink>,
) -> impl FnMut(&mut [[f32; 2]]) + Send + 'static {
    let mut voice = vec![0f32; dsp::BLOCK];
    let mut heard = vec![0f32; dsp::BLOCK];
    let mut limiter = Limiter::default();
    move |out| {
        let (voice, heard) = (&mut voice[..out.len()], &mut heard[..out.len()]);
        if status.deafened() {
            voice.fill(0.0);
            out.fill([0.0; 2]);
        } else {
            mixer.lock().unwrap().pull(voice);
            share.lock().unwrap().pull(out);
        }
        cues.mix(voice);
        for (o, v) in out.iter_mut().zip(voice.iter()) {
            *o = [o[0] + v, o[1] + v];
        }
        limiter.process(out);
        for (h, o) in heard.iter_mut().zip(out.iter()) {
            *h = (o[0] + o[1]) / 2.0;
        }
        played.push(heard);
    }
}

/// Ouvre la sortie `name` (`None`, ou débranchée : celle du système). `fill`
/// donne le son à jouer (gauche, droite), à 48 kHz, par blocs de 10 ms.
fn open_output(name: Option<&str>, fill: impl FnMut(&mut [[f32; 2]]) + Send + 'static, broken: &Broken) -> Opened {
    let host = cpal::default_host();
    let chosen = name.and_then(|name| find(host.output_devices().map(|d| d.into_iter()), name));
    let fallback = name.is_some() && chosen.is_none();
    let device = match chosen {
        Some(d) => d,
        None => host.default_output_device().context("aucune sortie audio trouvée")?,
    };
    let supported = device.default_output_config()?;
    let config: StreamConfig = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_output::<f32>(&device, config, fill, broken),
        SampleFormat::I16 => build_output::<i16>(&device, config, fill, broken),
        SampleFormat::U16 => build_output::<u16>(&device, config, fill, broken),
        other => return Err(anyhow!("format {other} non géré")),
    }?;
    stream.play()?;
    Ok((stream, fallback))
}

fn build_output<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: StreamConfig,
    mut fill: impl FnMut(&mut [[f32; 2]]) + Send + 'static,
    broken: &Broken,
) -> Result<Stream> {
    let channels = config.channels as usize;
    let mut stereo = share::Stereo::new(config.sample_rate);
    let stream = device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _| {
            let frames = data.len() / channels;
            for (frame, lr) in data.chunks_mut(channels).zip(stereo.pull(frames, &mut fill)) {
                for (c, out) in frame.iter_mut().enumerate() {
                    *out = T::from_sample(share::side(*lr, c, channels).clamp(-1.0, 1.0));
                }
            }
        },
        broken.on_error("haut-parleurs"),
        None,
    )?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use opus::{Application, Channels, Encoder};

    use super::*;

    /// Le son d'un partage regardé fait partie de la référence de
    /// l'annulation d'écho : sinon il repartirait par le micro.
    #[test]
    fn son_du_partage_dans_la_reference_d_echo() {
        let status = Arc::new(Status::new(&Prefs::default()));
        let share = Arc::new(Mutex::new(share::Player::default()));
        let mut enc = Encoder::new(dsp::RATE, Channels::Stereo, Application::Audio).unwrap();
        let mut packet = [0u8; 1500];
        for k in 0..12u16 {
            let pcm: Vec<f32> = (0..960)
                .flat_map(|i| {
                    let t = (k as usize * 960 + i) as f32 / dsp::RATE as f32;
                    [0.5 * (t * 440.0 * std::f32::consts::TAU).sin(), 0.0]
                })
                .collect();
            let len = enc.encode_float(&pcm, &mut packet).unwrap();
            share.lock().unwrap().push(7, k, &packet[..len]);
        }
        let played = Arc::new(Played::default());
        let mut fill = room_output(
            Arc::new(Mutex::new(Mixer::new())),
            share,
            status.clone(),
            played.clone(),
            Arc::new(cue::Sink::new()),
        );
        let mut out = vec![[0f32; 2]; dsp::BLOCK];
        let (mut heard, mut sent) = (Vec::new(), Vec::new());
        for _ in 0..12 {
            fill(&mut out);
            sent.extend(out.iter().map(|[l, r]| (l + r) / 2.0));
        }
        played.take(&mut heard);
        assert_eq!(heard.len(), sent.len());
        assert!(dsp::level_db(&heard[2400..]) > -20.0);
        assert!(heard.iter().zip(&sent).all(|(h, s)| (h - s).abs() < 1e-6));

        // Son coupé : rien n'est joué, rien à retirer (une fois sortie la
        // milliseconde que retient le limiteur).
        status.deafened.store(true, Ordering::Relaxed);
        fill(&mut out);
        fill(&mut out);
        played.take(&mut heard);
        let heard = &heard[heard.len() - out.len()..];
        assert!(out.iter().all(|s| *s == [0.0; 2]));
        assert!(heard.iter().all(|s| *s == 0.0));
    }
}
