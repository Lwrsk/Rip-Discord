//! Traitement du son, indépendant des cartes son : rééchantillonnage,
//! nettoyage du micro (écho, bruit, gain), trames Opus de 20 ms, porte de
//! bruit, tampon de gigue (avec masquage des pertes), mixage des voix reçues
//! et limiteur de la sortie.

use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    sync::Mutex,
    time::Instant,
};

use anyhow::Result;
use nnnoiseless::DenoiseState;
use opus::{Application, Bandwidth, Bitrate, Channels, Decoder, Encoder, Signal};
use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{AdaptiveDigital, EchoCanceller, GainController2, MaxProcessingRate, Pipeline},
};

use crate::core::protocol::PeerId;

/// Fréquence de travail (Opus), en mono.
pub const RATE: u32 = 48_000;
/// 20 ms à 48 kHz.
pub const FRAME: usize = 960;
/// 10 ms à 48 kHz : le pas du nettoyage du micro, et des blocs joués.
pub const BLOCK: usize = FRAME / 2;
/// Ce qui a été joué, gardé au plus (250 ms) si le micro ne le lit plus.
pub const MAX_PLAYED: usize = 25 * BLOCK;
/// Avance gardée au plus sur ce que joue la sortie (50 ms), pour le son des
/// autres applications : au-delà, l'écho ne serait plus en face de sa source.
const MAX_LEAD: usize = 5 * BLOCK;
/// Avance minimale et maximale du tampon de gigue, en trames de 20 ms.
const MIN_DEPTH: usize = 2;
const MAX_DEPTH: usize = 8;
/// Trames d'avance tolérées au-delà de la cible avant de rattraper le retard.
const SLACK: usize = 3;
/// Un paquet plus loin que ça dans le futur : on se recale dessus.
const MAX_AHEAD: usize = 50;
/// La porte reste ouverte un moment après la fin d'une phrase.
const HANGOVER: u32 = 15;
/// Trames d'avant l'ouverture de la porte envoyées avec la première : le
/// début des mots (attaques, « s », « f ») n'est pas coupé.
const PREROLL: usize = 2;
/// Débit de la voix : celui de Discord par défaut, transparent pour la voix
/// en Opus pleine bande (20 kHz).
pub const BITRATE: i32 = 64_000;
/// Pertes réseau prévues : la redondance d'Opus est taillée pour ça.
const EXPECTED_LOSS: i32 = 10;
/// Trames perdues comblées à la suite avant de considérer que l'autre s'est tu.
const MAX_CONCEAL: u32 = 3;
/// Retard (en trames) au-delà duquel un paquet « ancien » signale en fait une
/// numérotation repartie de zéro.
const LATE_WINDOW: u16 = 50;

/// Demi-longueur du filtre de rééchantillonnage, en échantillons d'entrée :
/// moins d'un tiers de milliseconde d'attente à 48 kHz.
const HALF_TAPS: usize = 24;
/// Positions fractionnaires précalculées entre deux échantillons d'entrée.
const PHASES: usize = 256;

/// Rééchantillonnage en flux continu, par interpolation à sinus cardinal
/// fenêtré (Kaiser) : pas de repliement ni de son étouffé, contrairement à une
/// interpolation linéaire. À fréquence égale, rien n'est touché.
pub struct Resampler {
    /// Pas, en échantillons d'entrée, entre deux échantillons de sortie.
    step: f64,
    /// `PHASES + 1` filtres de `2 × HALF_TAPS` coefficients ; vide si rien à faire.
    table: Vec<f32>,
    /// Entrée gardée : `HALF_TAPS` échantillons de passé, puis ce qui reste.
    history: Vec<f32>,
    /// Position du prochain échantillon de sortie dans `history`.
    pos: f64,
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let step = from as f64 / to as f64;
        if from == to {
            return Self {
                step,
                table: Vec::new(),
                history: Vec::new(),
                pos: 0.0,
            };
        }
        // Coupure juste sous la plus basse des deux moitiés de fréquence.
        let cutoff = 0.94 * (to as f64 / from as f64).min(1.0);
        let beta = 8.6;
        let bessel = |x: f64| {
            // I0, la fonction de Bessel modifiée de la fenêtre de Kaiser.
            let (mut sum, mut term) = (1.0, 1.0);
            for k in 1..32 {
                term *= (x / (2.0 * k as f64)).powi(2);
                sum += term;
            }
            sum
        };
        let norm = bessel(beta);
        let taps = 2 * HALF_TAPS;
        let mut table = Vec::with_capacity((PHASES + 1) * taps);
        for p in 0..=PHASES {
            let frac = p as f64 / PHASES as f64;
            let start = table.len();
            for j in 0..taps {
                // Distance entre l'échantillon d'entrée j et l'instant voulu.
                let t = j as f64 - (HALF_TAPS as f64 - 1.0) - frac;
                let x = t / HALF_TAPS as f64;
                let window = if x.abs() >= 1.0 {
                    0.0
                } else {
                    bessel(beta * (1.0 - x * x).sqrt()) / norm
                };
                let arg = std::f64::consts::PI * cutoff * t;
                let sinc = if arg.abs() < 1e-12 { 1.0 } else { arg.sin() / arg };
                table.push((cutoff * sinc * window) as f32);
            }
            // Gain exactement 1 à toutes les positions : pas de bourdonnement.
            let sum: f32 = table[start..].iter().sum();
            for c in &mut table[start..] {
                *c /= sum;
            }
        }
        Self {
            step,
            table,
            history: vec![0.0; HALF_TAPS],
            pos: HALF_TAPS as f64,
        }
    }

    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.table.is_empty() {
            out.extend_from_slice(input);
            return;
        }
        let taps = 2 * HALF_TAPS;
        self.history.extend_from_slice(input);
        // Le filtre regarde de `i - HALF_TAPS + 1` à `i + HALF_TAPS`.
        while (self.pos as usize) + HALF_TAPS < self.history.len() {
            let i = self.pos as usize;
            let phase = (self.pos - i as f64) * PHASES as f64;
            let p = (phase as usize).min(PHASES - 1);
            let f = (phase - p as f64) as f32;
            let x = &self.history[i + 1 - HALF_TAPS..=i + HALF_TAPS];
            let a = &self.table[p * taps..(p + 1) * taps];
            let b = &self.table[(p + 1) * taps..(p + 2) * taps];
            let (mut sa, mut sb) = (0.0f32, 0.0f32);
            for ((x, a), b) in x.iter().zip(a).zip(b) {
                sa += x * a;
                sb += x * b;
            }
            out.push(sa + (sb - sa) * f);
            self.pos += self.step;
        }
        // On ne garde que le passé dont le filtre aura encore besoin.
        let keep_from = (self.pos as usize + 1)
            .saturating_sub(HALF_TAPS)
            .min(self.history.len());
        self.history.drain(..keep_from);
        self.pos -= keep_from as f64;
    }
}

/// Niveau d'une trame en décibels (0 = saturation, ≈ -90 = silence).
pub fn level_db(samples: &[f32]) -> f32 {
    let energy = samples.iter().map(|s| s * s).sum::<f32>() / samples.len().max(1) as f32;
    10.0 * energy.max(1e-9).log10()
}

/// Étages du nettoyage du micro, chacun activable à la volée.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Clean {
    /// Bruit de fond retiré (ventilateur, clavier, souffle).
    pub noise: bool,
    /// Écho des haut-parleurs retiré : les autres ne s'entendent pas en retour.
    pub echo: bool,
    /// Gain automatique : une voix toujours au même niveau.
    pub gain: bool,
}

/// Nettoyage du micro par blocs de 10 ms, dans cet ordre : écho (il faut le
/// signal brut pour le reconnaître), bruit, puis gain. Les trois étages sont
/// créés d'avance : en couper ou en rallumer un ne coûte rien sur le fil du
/// son, et l'annulation d'écho garde ce qu'elle a appris.
pub struct Cleaner {
    pub on: Clean,
    /// AEC3 de WebRTC, avec son filtre passe-haut. Il retrouve seul le retard
    /// entre ce qui sort des haut-parleurs et ce qui revient dans le micro.
    echo: AudioProcessing,
    /// RNNoise : ventilateur et souffle, mais aussi clavier et clics.
    noise: Box<DenoiseState<'static>>,
    /// AGC2 de WebRTC.
    gain: AudioProcessing,
    /// Ce qui a été joué, pas encore donné à l'annulation d'écho (moins d'un bloc).
    far: Vec<f32>,
    block: [f32; BLOCK],
}

impl Default for Cleaner {
    fn default() -> Self {
        // Tout à 48 kHz : rien n'est coupé au-dessus de 16 kHz.
        let apm = |config: Config| {
            let pipeline = Pipeline {
                maximum_internal_processing_rate: MaxProcessingRate::Rate48kHz,
                ..Default::default()
            };
            AudioProcessing::builder()
                .config(Config { pipeline, ..config })
                .capture_config(StreamConfig::new(RATE, 1))
                .render_config(StreamConfig::new(RATE, 1))
                .build()
        };
        let gain = GainController2 {
            adaptive_digital: Some(AdaptiveDigital::default()),
            ..Default::default()
        };
        Self {
            on: Clean::default(),
            echo: apm(Config {
                echo_canceller: Some(EchoCanceller::default()),
                ..Default::default()
            }),
            noise: DenoiseState::new(),
            gain: apm(Config {
                gain_controller2: Some(gain),
                ..Default::default()
            }),
            far: Vec::with_capacity(MAX_PLAYED + BLOCK),
            block: [0.0; BLOCK],
        }
    }
}

impl Cleaner {
    /// Ce qui vient de sortir des haut-parleurs (48 kHz) : la référence de
    /// l'annulation d'écho, à donner avant le micro qui a pu l'entendre.
    pub fn far(&mut self, samples: &[f32]) {
        if !self.on.echo {
            self.far.clear();
            return;
        }
        self.far
            .extend_from_slice(&samples[samples.len().saturating_sub(MAX_PLAYED)..]);
        let mut used = 0;
        while self.far.len() - used >= BLOCK {
            let _ = self
                .echo
                .process_render_f32(&[&self.far[used..used + BLOCK]], &mut [&mut self.block[..]]);
            used += BLOCK;
        }
        self.far.drain(..used);
    }

    /// Nettoie sur place des blocs entiers de 10 ms.
    pub fn process(&mut self, samples: &mut [f32]) {
        for b in samples.as_chunks_mut::<BLOCK>().0 {
            if self.on.echo {
                self.block.copy_from_slice(b);
                let _ = self.echo.process_capture_f32(&[&self.block[..]], &mut [&mut *b]);
            }
            if self.on.noise {
                // RNNoise compte en échantillons 16 bits.
                for (x, s) in self.block.iter_mut().zip(b.iter()) {
                    *x = s * 32768.0;
                }
                self.noise.process_frame(b, &self.block);
                for s in b.iter_mut() {
                    *s = (*s / 32768.0).clamp(-1.0, 1.0);
                }
            }
            if self.on.gain {
                self.block.copy_from_slice(b);
                let _ = self.gain.process_capture_f32(&[&self.block[..]], &mut [&mut *b]);
            }
        }
    }
}

/// Anticipation du limiteur : 1 ms à 48 kHz.
const LOOKAHEAD: usize = 48;
/// Plafond du limiteur (-1 dB) : jamais d'écrêtage, même voix et volumes à fond.
const CEILING: f32 = 0.89;

/// Limiteur de la sortie (stéréo, 48 kHz) : plusieurs voix fortes, ou des
/// volumes poussés à 200 %, baissent en douceur au lieu de saturer. Il voit
/// venir les crêtes 1 ms à l'avance et ne touche pas à un son qui passe.
pub struct Limiter {
    /// Son retardé de [`LOOKAHEAD`], et gain voulu pour chaque échantillon.
    delay: VecDeque<[f32; 2]>,
    wanted: VecDeque<f32>,
    gain: f32,
}

impl Default for Limiter {
    fn default() -> Self {
        Self {
            delay: std::iter::repeat_n([0.0; 2], LOOKAHEAD).collect(),
            wanted: std::iter::repeat_n(1.0, LOOKAHEAD).collect(),
            gain: 1.0,
        }
    }
}

impl Limiter {
    pub fn process(&mut self, samples: &mut [[f32; 2]]) {
        // Descente bouclée pendant l'anticipation, remontée en ~100 ms.
        let attack = 1.0 - (-4.0 / LOOKAHEAD as f32).exp();
        let release = 1.0 - (-1.0 / (0.1 * RATE as f32)).exp();
        for s in samples.iter_mut() {
            let peak = s[0].abs().max(s[1].abs());
            self.delay.push_back(*s);
            self.wanted.push_back(if peak > CEILING { CEILING / peak } else { 1.0 });
            let out = self.delay.pop_front().unwrap_or_default();
            let own = self.wanted.pop_front().unwrap_or(1.0);
            // Le gain le plus bas demandé d'ici la fin de l'anticipation.
            let target = self.wanted.iter().copied().fold(own, f32::min);
            let rate = if target < self.gain { attack } else { release };
            self.gain += (target - self.gain) * rate;
            // Filet : l'échantillon qui sort ne dépasse jamais le plafond.
            let peak = out[0].abs().max(out[1].abs());
            let g = if peak * self.gain > CEILING {
                CEILING / peak
            } else {
                self.gain
            };
            *s = [out[0] * g, out[1] * g];
        }
    }
}

/// Ce qui vient de sortir des haut-parleurs (48 kHz), en attendant que le
/// micro, sur un autre fil, le lise pour l'annulation d'écho.
pub struct Played(Mutex<VecDeque<f32>>);

impl Default for Played {
    fn default() -> Self {
        Self(Mutex::new(VecDeque::with_capacity(MAX_PLAYED)))
    }
}

/// Ce que jouent les autres applications pendant qu'on partage leur son
/// (48 kHz, mono). Ça sort des haut-parleurs sans passer par nous : sans lui
/// dans la référence d'écho, le micro le renverrait, et les spectateurs
/// entendraient deux fois une vidéo ou un jeu (par le partage, puis par le micro).
pub static SHARED: Played = Played::new();

impl Played {
    pub const fn new() -> Self {
        Self(Mutex::new(VecDeque::new()))
    }

    /// Ajoute des échantillons stéréo entrelacés, ramenés en mono.
    pub fn push_stereo(&self, samples: &[f32]) {
        let mono: Vec<f32> = samples.as_chunks::<2>().0.iter().map(|[l, r]| (l + r) / 2.0).collect();
        self.push(&mono);
    }

    /// Ajoute à `out` (ce que la sortie vient de jouer) autant de ce qui
    /// attend, le plus ancien d'abord ; le reste, borné, attend la suite.
    pub fn mix_into(&self, out: &mut [f32]) {
        let mut q = self.0.lock().unwrap();
        let n = out.len().min(q.len());
        for (o, s) in out.iter_mut().zip(q.drain(..n)) {
            *o += s;
        }
        let excess = q.len().saturating_sub(MAX_LEAD);
        q.drain(..excess);
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    /// Côté sortie, sans allocation. Si le micro ne lit plus, le plus ancien
    /// est oublié.
    pub fn push(&self, samples: &[f32]) {
        let samples = &samples[samples.len().saturating_sub(MAX_PLAYED)..];
        let mut q = self.0.lock().unwrap();
        let excess = (q.len() + samples.len()).saturating_sub(MAX_PLAYED);
        q.drain(..excess);
        q.extend(samples);
    }

    /// Côté micro : tout ce qui a été joué depuis la dernière fois.
    pub fn take(&self, out: &mut Vec<f32>) {
        out.clear();
        out.extend(self.0.lock().unwrap().drain(..));
    }
}

/// Micro → trames Opus prêtes à partir.
pub struct Capture {
    resampler: Resampler,
    pending: Vec<f32>,
    /// Rien n'est nettoyé tant que [`Capture::set_clean`] ne l'a pas demandé.
    cleaner: Cleaner,
    encoder: Encoder,
    seq: u16,
    open_for: u32,
    /// Dernières trames encodées, porte fermée : de quoi ne pas couper le
    /// début de la phrase suivante.
    preroll: VecDeque<Vec<u8>>,
}

impl Capture {
    pub fn new(device_rate: u32) -> Result<Self> {
        let mut encoder = Encoder::new(RATE, Channels::Mono, Application::Voip)?;
        encoder.set_bitrate(Bitrate::Bits(BITRATE))?;
        encoder.set_max_bandwidth(Bandwidth::Fullband)?;
        encoder.set_signal(Signal::Voice)?;
        // Le meilleur encodage : une trame de voix ne coûte presque rien.
        encoder.set_complexity(10)?;
        encoder.set_inband_fec(true)?;
        encoder.set_packet_loss_perc(EXPECTED_LOSS)?;
        Ok(Self {
            resampler: Resampler::new(device_rate, RATE),
            pending: Vec::with_capacity(4 * FRAME),
            cleaner: Cleaner::default(),
            encoder,
            seq: 0,
            open_for: 0,
            preroll: VecDeque::with_capacity(PREROLL + 1),
        })
    }

    /// Étages du nettoyage à appliquer désormais.
    pub fn set_clean(&mut self, clean: Clean) {
        self.cleaner.on = clean;
    }

    /// Ce que les haut-parleurs viennent de jouer (48 kHz).
    pub fn played(&mut self, samples: &[f32]) {
        self.cleaner.far(samples);
    }

    /// Ajoute des échantillons mono du micro. Renvoie les trames à envoyer et
    /// le niveau de la dernière trame (une fois nettoyée : c'est elle qui part).
    pub fn push(&mut self, samples: &[f32], gate: Gate) -> (Vec<(u16, Vec<u8>)>, Option<f32>) {
        self.resampler.process(samples, &mut self.pending);
        let mut frames = Vec::new();
        let mut level = None;
        let mut out = [0u8; 1275];
        let mut frame = [0f32; FRAME];
        while self.pending.len() >= FRAME {
            frame.copy_from_slice(&self.pending[..FRAME]);
            self.pending.drain(..FRAME);
            self.cleaner.process(&mut frame);
            let db = level_db(&frame);
            level = Some(db);
            let was_open = self.open_for > 0;
            match gate {
                Gate::Closed => self.open_for = 0,
                // Touche maintenue : tout part, puis la porte se referme net.
                Gate::Open => self.open_for = 1,
                Gate::Voice(threshold) if db >= threshold => self.open_for = HANGOVER,
                Gate::Voice(_) => self.open_for = self.open_for.saturating_sub(1),
            }
            // Micro coupé ou touche relâchée : rien n'est encodé ni gardé, rien
            // d'entendu à ce moment-là ne partira plus tard.
            if gate == Gate::Closed {
                self.preroll.clear();
                continue;
            }
            // Porte fermée : la voix est quand même encodée (l'encodeur garde
            // le fil, la phrase suivante démarre net) et les dernières trames
            // gardées, pour partir avec le début de la phrase.
            let Ok(n) = self.encoder.encode_float(&frame, &mut out) else {
                continue;
            };
            if self.open_for == 0 {
                if self.preroll.len() == PREROLL {
                    self.preroll.pop_front();
                }
                self.preroll.push_back(out[..n].to_vec());
                continue;
            }
            if !was_open && matches!(gate, Gate::Voice(_)) {
                for opus in self.preroll.drain(..) {
                    frames.push((self.seq, opus));
                    self.seq = self.seq.wrapping_add(1);
                }
            }
            self.preroll.clear();
            frames.push((self.seq, out[..n].to_vec()));
            self.seq = self.seq.wrapping_add(1);
        }
        (frames, level)
    }
}

/// Ce qui décide si le micro part.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gate {
    /// Micro coupé (ou appui-pour-parler relâché).
    Closed,
    /// Touche d'appui-pour-parler ou de chuchotement maintenue.
    Open,
    /// Détection de la voix : au-dessus de ce niveau (dB), ça part.
    Voice(f32),
}

/// Estimation de la gigue (variation des délais d'arrivée, à la RFC 3550).
struct Jitter {
    ms: f32,
    /// Arrivée et numéro (déroulé) du dernier paquet.
    last: Option<(Instant, i64)>,
    /// Paquets à ne pas mesurer : ceux du début de phrase partent d'un coup.
    skip: usize,
}

impl Jitter {
    fn new() -> Self {
        Self {
            ms: 10.0,
            last: None,
            skip: 0,
        }
    }

    fn observe(&mut self, now: Instant, seq: i64) {
        match self.last {
            Some((at, prev)) if seq > prev => {
                let elapsed = now.duration_since(at).as_secs_f32() * 1000.0;
                let expected = (seq - prev) as f32 * 20.0;
                if elapsed - expected >= 300.0 {
                    // Long silence (porte de bruit fermée) : pas une mesure de
                    // gigue, et la phrase qui commence arrive en rafale.
                    self.skip = PREROLL;
                } else if self.skip > 0 {
                    self.skip -= 1;
                } else {
                    let d = (elapsed - expected).abs();
                    self.ms += (d - self.ms) / 16.0;
                }
            }
            Some(_) => {}
            None => self.skip = PREROLL,
        }
        if self.last.is_none_or(|(_, prev)| seq > prev) {
            self.last = Some((now, seq));
        }
    }

    /// Avance visée : assez pour absorber la gigue, pas plus.
    fn depth(&self) -> usize {
        (((self.ms * 2.0 + 15.0) / 20.0).ceil() as usize).clamp(MIN_DEPTH, MAX_DEPTH)
    }
}

/// La voix d'un participant : paquets rangés par numéro (ils peuvent arriver
/// dans le désordre ou pas du tout), décodés au moment de les jouer.
struct Voice {
    decoder: Decoder,
    /// `slots[0]` est la trame `next_seq` ; `None` = pas (encore) reçue.
    slots: VecDeque<Option<Vec<u8>>>,
    next_seq: u16,
    /// Numéro déroulé (sans retour à zéro) de `next_seq`, pour la gigue.
    next_ext: i64,
    /// Échantillons décodés pas encore joués.
    pcm: VecDeque<f32>,
    playing: bool,
    jitter: Jitter,
    /// Trames comblées d'affilée faute d'être arrivées à temps.
    concealed: u32,
}

impl Voice {
    fn new(decoder: Decoder, seq: u16) -> Self {
        Self {
            decoder,
            slots: VecDeque::new(),
            next_seq: seq,
            next_ext: 0,
            pcm: VecDeque::new(),
            playing: false,
            jitter: Jitter::new(),
            concealed: 0,
        }
    }

    fn resync(&mut self, seq: u16) {
        self.slots.clear();
        self.next_seq = seq;
        self.next_ext += MAX_AHEAD as i64 * 4;
        self.jitter.last = None;
    }

    fn push(&mut self, seq: u16, opus: &[u8], now: Instant) {
        let mut ahead = seq.wrapping_sub(self.next_seq);
        if ahead >= 0x8000 {
            // Déjà joué (paquet en retard) : trop tard. Un grand saut en arrière
            // veut dire que l'autre a relancé son micro (sa numérotation repart
            // de 0) : on se recale dessus.
            let late = 0u16.wrapping_sub(ahead);
            if late <= LATE_WINDOW {
                // Arrivé après son tour (déjà comblé) : il dit quand même
                // que le réseau traîne, le tampon grandira.
                self.jitter.observe(now, self.next_ext - late as i64);
                return;
            }
            self.resync(seq);
            ahead = 0;
        } else if ahead as usize >= MAX_AHEAD {
            self.resync(seq);
            ahead = 0;
        }
        let at = ahead as usize;
        while self.slots.len() <= at {
            self.slots.push_back(None);
        }
        self.slots[at] = Some(opus.to_vec());
        self.jitter.observe(now, self.next_ext + at as i64);
    }

    /// Trames d'avance (reçues ou attendues) avant la dernière reçue.
    fn buffered(&self) -> usize {
        self.slots.len() + self.pcm.len() / FRAME
    }

    /// Décode la trame suivante. Faux s'il n'y a plus rien à jouer.
    fn decode_next(&mut self, pcm: &mut [f32; 5760]) -> bool {
        let Some(slot) = self.slots.pop_front() else {
            return false;
        };
        self.next_seq = self.next_seq.wrapping_add(1);
        self.next_ext += 1;
        if slot.is_some() {
            self.concealed = 0;
        }
        let n = match slot {
            Some(opus) => self.decoder.decode_float(&opus, pcm, false),
            // Trame perdue : Opus la reconstitue grâce à la redondance de la
            // suivante si elle est là, sinon il la comble au mieux.
            // (la redondance n'existe qu'en mode « voix » d'Opus).
            None => match self.slots.front() {
                Some(Some(next)) => self
                    .decoder
                    .decode_float(next, &mut pcm[..FRAME], true)
                    .or_else(|_| self.decoder.decode_float(&[], &mut pcm[..FRAME], false)),
                _ => self.decoder.decode_float(&[], &mut pcm[..FRAME], false),
            },
        };
        if let Ok(n) = n {
            self.pcm.extend(&pcm[..n]);
        }
        true
    }

    /// Rien n'est arrivé à temps : le décodeur invente la suite (prolonge la
    /// voix en s'atténuant) au lieu d'un silence net, qui claquerait. Si le
    /// paquet finit par arriver, il est trop tard : la voix a continué sans.
    /// Faux après [`MAX_CONCEAL`] trames : l'autre s'est sans doute tu.
    fn conceal(&mut self, pcm: &mut [f32; 5760]) -> bool {
        if self.concealed >= MAX_CONCEAL {
            return false;
        }
        self.concealed += 1;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.next_ext += 1;
        let Ok(n) = self.decoder.decode_float(&[], &mut pcm[..FRAME], false) else {
            return false;
        };
        let frame = &mut pcm[..n];
        if self.concealed == MAX_CONCEAL {
            // Dernière trame inventée : fondu jusqu'au silence.
            let len = frame.len() as f32;
            for (i, s) in frame.iter_mut().enumerate() {
                *s *= 1.0 - i as f32 / len;
            }
        }
        self.pcm.extend(&*frame);
        true
    }

    /// La voix s'arrête (fin de phrase) : les trames comblées en attendant
    /// n'ont pas eu lieu, la phrase suivante reprend à leur numéro.
    fn stop(&mut self) {
        self.playing = false;
        self.next_seq = self.next_seq.wrapping_sub(self.concealed as u16);
        self.next_ext -= self.concealed as i64;
        self.concealed = 0;
    }
}

/// Voix reçues → un seul flux à 48 kHz pour les haut-parleurs.
#[derive(Default)]
pub struct Mixer {
    voices: HashMap<PeerId, Voice>,
    pub volume: f32,
    /// Volume propre à certains participants (1 = normal).
    gains: HashMap<PeerId, f32>,
}

impl Mixer {
    pub fn new() -> Self {
        Self {
            voices: HashMap::new(),
            volume: 1.0,
            gains: HashMap::new(),
        }
    }

    pub fn set_gain(&mut self, peer: PeerId, gain: f32) {
        self.gains.insert(peer, gain);
    }

    pub fn push(&mut self, from: PeerId, seq: u16, opus: &[u8]) {
        self.push_at(from, seq, opus, Instant::now());
    }

    /// Range une trame reçue à l'instant `now`.
    pub fn push_at(&mut self, from: PeerId, seq: u16, opus: &[u8], now: Instant) {
        let v = match self.voices.entry(from) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let Ok(decoder) = Decoder::new(RATE, Channels::Mono) else {
                    return;
                };
                e.insert(Voice::new(decoder, seq))
            }
        };
        v.push(seq, opus, now);
    }

    /// Remplit `out` avec le mélange des voix (silence s'il n'y en a pas).
    pub fn pull(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        let mut pcm = [0f32; 5760];
        for (id, v) in self.voices.iter_mut() {
            let gain = self.gains.get(id).copied().unwrap_or(1.0);
            let depth = v.jitter.depth();
            if !v.playing && v.buffered() >= depth {
                v.playing = true;
            }
            if !v.playing {
                continue;
            }
            // Trop d'avance (rafale après un trou réseau) : on saute des trames
            // plutôt que de garder ce retard toute la conversation.
            // Les trames sautées passent quand même par le décodeur, qui garde
            // ainsi le fil (sinon la suite démarre sur un son étouffé).
            while v.slots.len() > depth + SLACK {
                v.decode_next(&mut pcm);
                v.pcm.clear();
            }
            while v.pcm.len() < out.len() && (v.decode_next(&mut pcm) || v.conceal(&mut pcm)) {}
            let n = out.len().min(v.pcm.len());
            for (o, s) in out.iter_mut().zip(v.pcm.drain(..n)) {
                *o += s * gain;
            }
            if v.pcm.is_empty() && v.slots.is_empty() && v.concealed >= MAX_CONCEAL {
                // Fin de phrase ou réseau très en retard : on réattend un peu d'avance.
                v.stop();
            }
        }
        // Pas d'écrêtage ici : le limiteur de la sortie s'en charge en douceur.
        for o in out.iter_mut() {
            *o *= self.volume;
        }
    }

    /// Oublie les voix de ceux qui ne sont plus dans le salon.
    pub fn retain(&mut self, members: &[PeerId]) {
        self.voices.retain(|id, _| members.contains(id));
    }

    /// Avance actuelle du tampon de `peer`, en millisecondes (diagnostic).
    pub fn delay_ms(&self, peer: PeerId) -> Option<u32> {
        self.voices.get(&peer).map(|v| (v.jitter.depth() * 20) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, freq: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| 0.5 * (i as f32 * freq * std::f32::consts::TAU / rate as f32).sin())
            .collect()
    }

    #[test]
    fn reechantillonnage_garde_la_duree() {
        let input = sine(44_100, 440.0, 1.0);
        let mut r = Resampler::new(44_100, RATE);
        let mut out = Vec::new();
        for chunk in input.chunks(441) {
            r.process(chunk, &mut out);
        }
        // Il manque seulement l'attente du filtre (moins d'une demi-milliseconde).
        let missing = RATE as i64 - out.len() as i64;
        assert!((0..=2 * HALF_TAPS as i64).contains(&missing), "{}", out.len());
    }

    /// Un son aigu rééchantillonné reste propre : ni repliement ni bruit
    /// d'interpolation (à -40 dB environ avec une interpolation linéaire).
    #[test]
    fn reechantillonnage_propre() {
        for (from, to) in [(44_100, RATE), (RATE, 44_100), (16_000, RATE)] {
            let freq = 0.3 * from.min(to) as f32;
            let input: Vec<f32> = (0..from)
                .map(|i| 0.5 * (i as f64 * freq as f64 * std::f64::consts::TAU / from as f64).sin() as f32)
                .collect();
            let mut r = Resampler::new(from, to);
            let mut out = Vec::new();
            for chunk in input.chunks(from as usize / 100) {
                r.process(chunk, &mut out);
            }
            let ideal = |i: usize| 0.5 * (i as f64 * freq as f64 * std::f64::consts::TAU / to as f64).sin() as f32;
            let tail = &out[to as usize / 10..out.len() - 100];
            let error: Vec<f32> = tail
                .iter()
                .enumerate()
                .map(|(i, s)| s - ideal(i + to as usize / 10))
                .collect();
            let snr = level_db(tail) - level_db(&error);
            assert!(snr > 70.0, "{from} → {to} Hz : {snr:.1} dB");
        }
        // Même fréquence : rien ne change, rien n'attend.
        let input = sine(RATE, 440.0, 0.1);
        let mut out = Vec::new();
        Resampler::new(RATE, RATE).process(&input, &mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn la_voix_passe_le_silence_non() {
        let mut capture = Capture::new(RATE).unwrap();
        let (frames, _) = capture.push(&vec![0.0; FRAME * 25], Gate::Voice(-50.0));
        assert!(frames.is_empty(), "silence envoyé");
        let (frames, level) = capture.push(&sine(RATE, 300.0, 0.2), Gate::Voice(-50.0));
        // Les deux trames d'avant partent avec la première : le début du mot.
        assert_eq!(frames.len(), PREROLL + 10);
        assert!(frames.windows(2).all(|w| w[1].0 == w[0].0.wrapping_add(1)));
        assert!(level.unwrap() > -10.0);
        let (frames, _) = capture.push(&sine(RATE, 300.0, 0.2), Gate::Closed);
        assert!(frames.is_empty(), "micro coupé");
        // Appui-pour-parler : même le silence part tant que la touche est tenue.
        let (frames, _) = capture.push(&vec![0.0; FRAME * 5], Gate::Open);
        assert_eq!(frames.len(), 5);
    }

    #[test]
    fn mixage_apres_decodage() {
        let mut capture = Capture::new(RATE).unwrap();
        let (frames, _) = capture.push(&sine(RATE, 300.0, 0.2), Gate::Voice(-50.0));
        let mut mixer = Mixer::new();
        let mut out = vec![0.0; FRAME];
        // Pas assez d'avance : silence.
        mixer.push(1, frames[0].0, &frames[0].1);
        mixer.pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        for (seq, opus) in &frames[1..] {
            mixer.push(1, *seq, opus);
        }
        mixer.pull(&mut out);
        assert!(level_db(&out) > -20.0, "{}", level_db(&out));
        mixer.retain(&[]);
        mixer.pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn micro_relance_entendu_tout_de_suite() {
        let mut capture = Capture::new(RATE).unwrap();
        let (frames, _) = capture.push(&sine(RATE, 300.0, 0.2), Gate::Voice(-50.0));
        let mut mixer = Mixer::new();
        let mut out = vec![0.0; FRAME];
        // La voix en était à la trame 5000 quand le micro a été changé.
        mixer.push(1, 5000, &frames[0].1);
        mixer.pull(&mut out);
        // Nouveau micro : la numérotation repart de 0.
        for (seq, opus) in &frames {
            mixer.push(1, *seq, opus);
        }
        mixer.pull(&mut out);
        assert!(level_db(&out) > -20.0, "{}", level_db(&out));
    }

    fn voice_frames(n: usize) -> Vec<(u16, Vec<u8>)> {
        let mut capture = Capture::new(RATE).unwrap();
        // Une demi-trame de plus : l'arrondi ne doit pas en faire perdre une.
        let (frames, _) = capture.push(&sine(RATE, 300.0, (n as f32 + 0.5) * 0.02), Gate::Open);
        assert_eq!(frames.len(), n);
        frames
    }

    #[test]
    fn paquets_dans_le_desordre_remis_dans_l_ordre() {
        let frames = voice_frames(6);
        let mut mixer = Mixer::new();
        let now = Instant::now();
        // 0, 2, 1, 3 : la trame 1 arrive après la 2, elle est quand même jouée.
        for i in [0, 2, 1, 3] {
            mixer.push_at(1, frames[i].0, &frames[i].1, now);
        }
        let v = &mixer.voices[&1];
        assert!(v.slots.iter().all(Option::is_some), "trou dans le tampon");
        let mut out = vec![0.0; FRAME];
        mixer.pull(&mut out);
        assert!(level_db(&out) > -20.0);
        // Une trame déjà jouée qui arrive enfin est ignorée.
        mixer.push_at(1, frames[0].0, &frames[0].1, now);
        assert_eq!(mixer.voices[&1].next_seq, 1);
    }

    #[test]
    fn trame_perdue_comblee() {
        let frames = voice_frames(5);
        let mut mixer = Mixer::new();
        let now = Instant::now();
        for (seq, opus) in frames.iter().filter(|(s, _)| *s != 2) {
            mixer.push_at(1, *seq, opus, now);
        }
        let mut out = vec![0.0; FRAME];
        let mut played = 0;
        for _ in 0..5 {
            mixer.pull(&mut out);
            if level_db(&out) > -30.0 {
                played += 1;
            }
        }
        // Les 5 trames sont jouées, la perdue reconstituée.
        assert_eq!(played, 5);
        // Puis la voix s'éteint en fondu, et c'est le silence.
        for _ in 0..MAX_CONCEAL {
            mixer.pull(&mut out);
        }
        assert!(out.last().unwrap().abs() < 1e-3);
        mixer.pull(&mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    /// Le réseau traîne au milieu d'une phrase : la voix continue (le
    /// décodeur invente la suite) au lieu de couper net, puis reprend.
    #[test]
    fn retard_reseau_comble_sans_coupure() {
        let frames = voice_frames(12);
        let mut mixer = Mixer::new();
        let now = Instant::now();
        for (seq, opus) in &frames[..4] {
            mixer.push_at(1, *seq, opus, now);
        }
        let mut out = vec![0.0; FRAME];
        for _ in 0..4 {
            mixer.pull(&mut out);
        }
        // Plus rien d'arrivé : la trame suivante est inventée, pas du silence.
        mixer.pull(&mut out);
        assert!(level_db(&out) > -30.0, "{}", level_db(&out));
        // La trame 4 arrive enfin, trop tard ; la suite est jouée tout de suite.
        for (seq, opus) in &frames[4..] {
            mixer.push_at(1, *seq, opus, now);
        }
        assert_eq!(mixer.voices[&1].slots.len(), 7);
        mixer.pull(&mut out);
        assert!(level_db(&out) > -20.0, "{}", level_db(&out));
    }

    /// Fin de phrase : les trames inventées en attendant ne décalent pas la
    /// phrase suivante, qui est jouée en entier.
    #[test]
    fn phrase_suivante_apres_un_silence() {
        let frames = voice_frames(10);
        let mut mixer = Mixer::new();
        let now = Instant::now();
        for (seq, opus) in &frames[..4] {
            mixer.push_at(1, *seq, opus, now);
        }
        let mut out = vec![0.0; FRAME];
        for _ in 0..4 + MAX_CONCEAL + 2 {
            mixer.pull(&mut out);
        }
        assert!(!mixer.voices[&1].playing);
        assert_eq!(mixer.voices[&1].next_seq, 4);
        for (seq, opus) in &frames[4..] {
            mixer.push_at(1, *seq, opus, now + std::time::Duration::from_secs(1));
        }
        assert_eq!(mixer.voices[&1].slots.len(), 6);
        mixer.pull(&mut out);
        assert!(level_db(&out) > -20.0, "{}", level_db(&out));
    }

    #[test]
    fn micro_coupe_rien_d_avant_ne_part() {
        let mut capture = Capture::new(RATE).unwrap();
        // Parlé micro coupé, puis rétabli pendant un silence.
        capture.push(&sine(RATE, 300.0, 0.2), Gate::Closed);
        let (frames, _) = capture.push(&sine(RATE, 300.0, 0.02), Gate::Voice(-50.0));
        assert_eq!(frames.len(), 1, "des trames d'avant le micro rétabli sont parties");
        // Appui-pour-parler : rien d'avant l'appui.
        let mut capture = Capture::new(RATE).unwrap();
        capture.push(&sine(RATE, 300.0, 0.2), Gate::Closed);
        let (frames, _) = capture.push(&sine(RATE, 300.0, 0.02), Gate::Open);
        assert_eq!(frames.len(), 1);
    }

    /// La voix part en Opus hybride pleine bande : jusqu'à 20 kHz, avec la
    /// redondance qui reconstitue une trame perdue.
    #[test]
    fn voix_pleine_bande_avec_redondance() {
        let mut capture = Capture::new(RATE).unwrap();
        let (frames, _) = capture.push(&voice(1.0, 140.0), Gate::Open);
        assert_eq!(frames.len(), 50);
        for (_, opus) in &frames[5..] {
            // Configuration 15 : hybride, pleine bande, 20 ms.
            assert_eq!(opus[0] >> 3, 15);
        }
    }

    #[test]
    fn limiteur_sans_saturation_ni_coloration() {
        let mut limiter = Limiter::default();
        // Deux voix fortes et un volume à 200 % : 2,4 en crête.
        let loud: Vec<[f32; 2]> = voice(1.0, 140.0)
            .iter()
            .zip(voice(1.0, 220.0))
            .map(|(a, b)| {
                let s = (a + b) * 4.0;
                [s, s]
            })
            .collect();
        assert!(loud.iter().any(|s| s[0] > 1.5));
        let mut out = loud.clone();
        for block in out.chunks_mut(BLOCK) {
            limiter.process(block);
        }
        assert!(out.iter().all(|s| s[0].abs() <= CEILING + 1e-6));
        // Un son normal passe tel quel (1 ms plus tard).
        let mut limiter = Limiter::default();
        let quiet: Vec<[f32; 2]> = voice(0.5, 140.0).iter().map(|s| [*s, -*s]).collect();
        let mut out = quiet.clone();
        limiter.process(&mut out);
        assert!(out[LOOKAHEAD..].iter().zip(&quiet).all(|(o, q)| o == q));
    }

    #[test]
    fn rafale_rattrapee_sans_garder_le_retard() {
        let frames = voice_frames(40);
        let mut mixer = Mixer::new();
        let now = Instant::now();
        for (seq, opus) in &frames {
            mixer.push_at(1, *seq, opus, now);
        }
        let mut out = vec![0.0; FRAME];
        mixer.pull(&mut out);
        let v = &mixer.voices[&1];
        assert!(v.slots.len() <= MAX_DEPTH + SLACK, "{}", v.slots.len());
    }

    #[test]
    fn gigue_mesuree_tampon_adapte() {
        let frames = voice_frames(40);
        let mut mixer = Mixer::new();
        let t0 = Instant::now();
        // Réseau régulier : avance minimale.
        for (i, (seq, opus)) in frames[..20].iter().enumerate() {
            mixer.push_at(1, *seq, opus, t0 + std::time::Duration::from_millis(20 * i as u64));
        }
        assert_eq!(mixer.delay_ms(1), Some(40));
        // Paquets qui arrivent par paquets de 4 (Wi-Fi chargé) : le tampon grandit.
        let mut mixer = Mixer::new();
        for (i, (seq, opus)) in frames.iter().enumerate() {
            let at = (i / 4 * 4 + 3) as u64 * 20 + if i % 8 == 0 { 25 } else { 0 };
            mixer.push_at(1, *seq, opus, t0 + std::time::Duration::from_millis(at));
        }
        assert!(mixer.delay_ms(1).unwrap() > 40, "{:?}", mixer.delay_ms(1));
    }

    /// Bruit de fond : souffle (bruit blanc adouci) et frappes de clavier.
    fn noise(secs: f32, clicks: bool) -> Vec<f32> {
        let mut seed = 1u32;
        let mut rand = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 23) as f32 - 1.0
        };
        let mut low = 0.0;
        (0..(RATE as f32 * secs) as usize)
            .map(|i| {
                low += (rand() - low) * 0.2;
                // Une frappe toutes les 150 ms.
                let t = i % 7200;
                let click = if clicks && t < 200 {
                    rand() * 0.3 * (-(t as f32) / 30.0).exp()
                } else {
                    0.0
                };
                low * 0.05 + click
            })
            .collect()
    }

    /// Voix synthétique : harmoniques d'une fondamentale qui bouge, en syllabes.
    fn voice(secs: f32, pitch: f32) -> Vec<f32> {
        let tau = std::f32::consts::TAU;
        (0..(RATE as f32 * secs) as usize)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let syllable = (t * 2.5 * tau).sin().max(0.0).sqrt();
                let f0 = pitch + 30.0 * (t * 3.0).sin();
                syllable * (1..8).map(|k| (t * f0 * k as f32 * tau).sin() / k as f32).sum::<f32>() * 0.15
            })
            .collect()
    }

    #[test]
    fn suppression_du_bruit_sur_du_bruit_seul() {
        for clicks in [false, true] {
            let mut cleaner = Cleaner::default();
            cleaner.on.noise = true;
            let mut samples = noise(2.0, clicks);
            let before = level_db(&samples[RATE as usize..]);
            cleaner.process(&mut samples);
            // La seconde d'après : le temps de reconnaître le bruit.
            let after = level_db(&samples[RATE as usize..]);
            assert!(after < before - 8.0, "frappes {clicks} : {before} dB → {after} dB");
        }
    }

    /// Les haut-parleurs jouent la voix d'un autre ; le micro la réentend
    /// 60 ms plus tard, plus faible.
    fn echo_of(far: &[f32]) -> Vec<f32> {
        let delay = RATE as usize * 60 / 1000;
        (0..far.len())
            .map(|i| if i >= delay { 0.5 * far[i - delay] } else { 0.0 })
            .collect()
    }

    /// Comme sur les fils du son : 20 ms jouées, puis 20 ms de micro.
    fn cancel(cleaner: &mut Cleaner, far: &[f32], mic: &mut [f32]) {
        for (played, frame) in far.chunks(FRAME).zip(mic.chunks_mut(FRAME)) {
            cleaner.far(played);
            cleaner.process(frame);
        }
    }

    #[test]
    fn annulation_d_echo_sur_un_echo_pur() {
        let mut cleaner = Cleaner::default();
        cleaner.on.echo = true;
        let far: Vec<f32> = voice(6.0, 140.0)
            .iter()
            .zip(noise(6.0, false))
            .map(|(v, n)| v + 0.2 * n)
            .collect();
        let mut mic = echo_of(&far);
        let tail = 4 * RATE as usize;
        let before = level_db(&mic[tail..]);
        cancel(&mut cleaner, &far, &mut mic);
        let after = level_db(&mic[tail..]);
        assert!(after < before - 30.0, "{before} dB → {after} dB");
    }

    #[test]
    fn annulation_d_echo_garde_ma_voix() {
        let mut cleaner = Cleaner::default();
        cleaner.on.echo = true;
        let far: Vec<f32> = voice(6.0, 140.0)
            .iter()
            .zip(noise(6.0, false))
            .map(|(v, n)| v + 0.2 * n)
            .collect();
        // Je parle en même temps que l'écho revient.
        let mine = voice(6.0, 220.0);
        let mut mic: Vec<f32> = echo_of(&far).iter().zip(&mine).map(|(e, m)| e + m).collect();
        cancel(&mut cleaner, &far, &mut mic);
        let tail = 4 * RATE as usize;
        let (expected, got) = (level_db(&mine[tail..]), level_db(&mic[tail..]));
        assert!((got - expected).abs() < 4.0, "{expected} dB attendus, {got} dB");
    }

    #[test]
    fn nettoyage_coupe_rien_ne_change() {
        let mut cleaner = Cleaner::default();
        let input = noise(0.5, true);
        let mut samples = input.clone();
        cleaner.far(&input);
        cleaner.process(&mut samples);
        assert_eq!(samples, input);
    }

    #[test]
    fn nettoyage_complet_reste_borne() {
        let mut capture = Capture::new(RATE).unwrap();
        capture.set_clean(Clean {
            noise: true,
            echo: true,
            gain: true,
        });
        let far = voice(1.0, 140.0);
        let mic: Vec<f32> = voice(1.0, 220.0)
            .iter()
            .zip(noise(1.0, true))
            .map(|(v, n)| (v + n) * 3.0)
            .collect();
        for (played, chunk) in far.chunks(FRAME).zip(mic.chunks(FRAME)) {
            capture.played(played);
            let (frames, level) = capture.push(chunk, Gate::Open);
            assert_eq!(frames.len(), 1);
            assert!(level.is_some_and(|db| db.is_finite() && db <= 0.0), "{level:?}");
        }
    }

    #[test]
    fn reference_d_echo_bornee() {
        let played = Played::default();
        let mut out = Vec::new();
        played.push(&[0.1; BLOCK]);
        played.take(&mut out);
        assert_eq!(out.len(), BLOCK);
        // Micro arrêté : seul le plus récent reste.
        for i in 0..100 {
            played.push(&[i as f32; BLOCK]);
        }
        played.take(&mut out);
        assert_eq!(out.len(), MAX_PLAYED);
        assert_eq!(out.last(), Some(&99.0));
        assert_eq!(out[0], (100 - MAX_PLAYED / BLOCK) as f32);
        played.take(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn son_des_autres_applications_dans_la_reference() {
        let shared = Played::new();
        // Gauche et droite mêlées, ajoutées à ce que la sortie a joué.
        shared.push_stereo(&[0.2, 0.4].repeat(BLOCK));
        let mut far = vec![0.1; BLOCK / 2];
        shared.mix_into(&mut far);
        assert!(far.iter().all(|s| (s - 0.4).abs() < 1e-6));
        // Le reste attend la suite, sans prendre trop d'avance.
        shared.push(&vec![0.5; 10 * BLOCK]);
        let mut far = vec![0.0; BLOCK];
        shared.mix_into(&mut far);
        assert!((far[0] - 0.3).abs() < 1e-6 && (far[BLOCK - 1] - 0.5).abs() < 1e-6);
        let mut rest = Vec::new();
        shared.take(&mut rest);
        assert_eq!(rest.len(), MAX_LEAD);
    }
}
