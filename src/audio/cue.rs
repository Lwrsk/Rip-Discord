//! Sons d'interface, comme sur Discord : quelqu'un entre dans le salon ou en
//! sort, micro ou son coupé, message ou mention. De petits sons créés par le
//! code (pas de fichiers), joués chez moi seulement.
//!
//! Dans un salon vocal, ils passent par la sortie du salon : ils font donc
//! partie de la référence de l'annulation d'écho, et mon micro ne les renvoie
//! pas aux autres. Hors d'un salon, une sortie s'ouvre le temps de les jouer.
//!
//! De n'importe où (interface, réseau) : `audio::Cue::Mention.play()`.

use std::{
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use tracing::warn;

use super::{Prefs, dsp::RATE};

/// Un son d'interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    /// Quelqu'un entre dans mon salon vocal (ou j'y entre).
    Join,
    /// Quelqu'un sort de mon salon vocal (ou j'en sors).
    Leave,
    Mute,
    Unmute,
    /// Son coupé (casque).
    Deafen,
    Undeafen,
    /// Nouveau message.
    Message,
    /// Quelqu'un parle de moi.
    Mention,
}

const ALL: [Cue; 8] = [
    Cue::Join,
    Cue::Leave,
    Cue::Mute,
    Cue::Unmute,
    Cue::Deafen,
    Cue::Undeafen,
    Cue::Message,
    Cue::Mention,
];

/// Crête des sons, avant le volume choisi.
const PEAK: f32 = 0.5;
/// Sons joués en même temps, au plus.
const VOICES: usize = 4;
/// La sortie à part reste ouverte un moment, pour enchaîner sans la rouvrir.
const LINGER: Duration = Duration::from_secs(3);
/// Numéro de la sortie à part (les sorties de salon comptent à partir de 1).
const ALONE: u64 = 0;

// Notes (Hz).
const G4: f32 = 392.0;
const D5: f32 = 587.33;
const E5: f32 = 659.25;
const G5: f32 = 783.99;
const A5: f32 = 880.0;
const C6: f32 = 1046.5;
const E6: f32 = 1318.51;

impl Cue {
    /// Joue le son chez moi, sans attendre. Rien si les sons d'interface sont
    /// coupés dans mes réglages.
    pub fn play(self) {
        let samples = self.samples();
        let alone = {
            let mut p = PLAYER.lock().unwrap();
            if !p.on {
                return;
            }
            p.start(samples);
            p.sinks.is_empty()
        };
        if alone {
            open_alone();
        }
    }

    /// Les échantillons du son (48 kHz, mono), créés une fois pour toutes.
    pub fn samples(self) -> &'static [f32] {
        static BANK: OnceLock<Vec<Vec<f32>>> = OnceLock::new();
        let bank = BANK.get_or_init(|| ALL.iter().map(|c| c.render()).collect());
        &bank[self as usize]
    }

    /// Notes du son : début (s), fréquence au départ et à l'arrivée (Hz), durée (s).
    fn notes(self) -> &'static [(f32, f32, f32, f32)] {
        match self {
            // Deux notes qui montent : bonjour ; qui descendent : au revoir.
            Cue::Join => &[(0.0, E5, E5, 0.16), (0.07, A5, A5, 0.24)],
            Cue::Leave => &[(0.0, A5, A5, 0.16), (0.07, E5, E5, 0.24)],
            // Une petite glissade, plus grave pour le casque.
            Cue::Mute => &[(0.0, G5, D5, 0.13)],
            Cue::Unmute => &[(0.0, D5, G5, 0.13)],
            Cue::Deafen => &[(0.0, D5, D5, 0.14), (0.06, G4, G4, 0.22)],
            Cue::Undeafen => &[(0.0, G4, G4, 0.14), (0.06, D5, D5, 0.22)],
            Cue::Message => &[(0.0, C6, C6, 0.26)],
            // Un petit arpège clair : on ne le confond pas avec un simple message.
            Cue::Mention => &[(0.0, G5, G5, 0.18), (0.08, C6, C6, 0.18), (0.16, E6, E6, 0.3)],
        }
    }

    /// Synthèse : des sinusoïdes douces (attaque de 5 ms, extinction
    /// exponentielle jusqu'à zéro, sans clic), avec un peu d'harmonique 2
    /// pour la rondeur.
    fn render(self) -> Vec<f32> {
        let rate = RATE as f32;
        let notes = self.notes();
        let len = notes.iter().map(|(at, _, _, d)| at + d).fold(0.0, f32::max);
        let mut out = vec![0f32; (len * rate) as usize];
        for &(at, from, to, dur) in notes {
            let start = (at * rate) as usize;
            let mut phase = 0f32;
            for (i, s) in out[start..].iter_mut().take((dur * rate) as usize).enumerate() {
                let t = i as f32 / rate;
                let f = from + (to - from) * (t / dur);
                phase = (phase + f / rate * std::f32::consts::TAU) % std::f32::consts::TAU;
                let attack = (t / 0.005).min(1.0);
                let decay = (-t / (dur * 0.3)).exp() * (1.0 - t / dur);
                *s += (phase.sin() + 0.2 * (2.0 * phase).sin()) * attack * decay;
            }
        }
        let peak = out.iter().fold(0f32, |m, s| m.max(s.abs()));
        for s in &mut out {
            *s *= PEAK / peak.max(1e-6);
        }
        out
    }
}

/// Mes réglages : sons activés ou non, leur volume, et la sortie à ouvrir
/// hors d'un salon vocal.
pub fn configure(prefs: &Prefs) {
    let mut p = PLAYER.lock().unwrap();
    p.on = prefs.cues;
    p.volume = prefs.cues_volume;
    if p.output != prefs.output {
        p.output = prefs.output.clone();
    }
    if !p.on {
        p.playing = [None; VOICES];
    }
}

static PLAYER: Mutex<Player> = Mutex::new(Player::new());
/// La sortie à part est ouverte (ou en train de s'ouvrir).
static OPEN_ALONE: AtomicBool = AtomicBool::new(false);

/// Les sons en cours, et qui les joue.
struct Player {
    on: bool,
    volume: f32,
    /// Sortie choisie dans mes réglages (`None` : celle du système).
    output: Option<String>,
    /// Sons en cours : échantillons et position.
    playing: [Option<(&'static [f32], usize)>; VOICES],
    /// Sorties des salons vocaux ouvertes : la plus récente joue les sons.
    sinks: Vec<u64>,
    next: u64,
}

impl Player {
    const fn new() -> Self {
        Self {
            on: true,
            volume: 0.6,
            output: None,
            playing: [None; VOICES],
            sinks: Vec::new(),
            next: ALONE,
        }
    }

    fn start(&mut self, samples: &'static [f32]) {
        // Le même son relancé repart du début ; sinon une place libre, ou la première.
        let slot = self
            .playing
            .iter()
            .position(|p| p.is_some_and(|(s, _)| std::ptr::eq(s, samples)))
            .or_else(|| self.playing.iter().position(Option::is_none))
            .unwrap_or(0);
        self.playing[slot] = Some((samples, 0));
    }

    fn busy(&self) -> bool {
        self.playing.iter().any(Option::is_some)
    }

    /// Ajoute à `out` (48 kHz) la suite des sons en cours, si c'est à la
    /// sortie `id` de les jouer. Sans allocation.
    fn mix(&mut self, id: u64, out: &mut [f32]) {
        let mine = self.sinks.last().map_or(id == ALONE, |last| *last == id);
        if !mine || !self.busy() {
            return;
        }
        for slot in &mut self.playing {
            let Some((samples, pos)) = slot else { continue };
            let n = (samples.len() - *pos).min(out.len());
            for (o, s) in out.iter_mut().zip(&samples[*pos..*pos + n]) {
                *o += s * self.volume;
            }
            *pos += n;
            if *pos == samples.len() {
                *slot = None;
            }
        }
        for o in out.iter_mut() {
            *o = o.clamp(-1.0, 1.0);
        }
    }
}

/// La sortie d'un salon vocal : c'est elle qui joue les sons d'interface tant
/// qu'elle vit (et qu'elle n'est pas retirée).
pub(super) struct Sink(u64);

impl Sink {
    pub(super) fn new() -> Self {
        let mut p = PLAYER.lock().unwrap();
        p.next += 1;
        let id = p.next;
        p.sinks.push(id);
        Self(id)
    }

    pub(super) fn id(&self) -> u64 {
        self.0
    }

    /// Ajoute les sons en cours à `out` (48 kHz), sur le fil de la sortie.
    pub(super) fn mix(&self, out: &mut [f32]) {
        PLAYER.lock().unwrap().mix(self.0, out);
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        retire(self.0);
    }
}

/// La sortie de salon `id` ne joue plus les sons (salon quitté) : ceux en
/// cours se terminent sur une sortie à part.
pub(super) fn retire(id: u64) {
    let alone = {
        let mut p = PLAYER.lock().unwrap();
        p.sinks.retain(|s| *s != id);
        p.sinks.is_empty() && p.busy()
    };
    if alone {
        open_alone();
    }
}

/// Ouvre la sortie à part sur son propre fil, si elle ne l'est pas déjà.
fn open_alone() {
    if OPEN_ALONE.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("sons".into()).spawn(|| {
        loop {
            play_alone();
            OPEN_ALONE.store(false, Ordering::Release);
            // Un son arrivé pendant la fermeture : on rouvre.
            let again = {
                let p = PLAYER.lock().unwrap();
                p.busy() && p.sinks.is_empty()
            };
            if !again || OPEN_ALONE.swap(true, Ordering::AcqRel) {
                break;
            }
        }
    });
    if let Err(e) = spawned {
        warn!("sons d'interface : {e}");
        OPEN_ALONE.store(false, Ordering::Release);
    }
}

/// Joue les sons tant qu'il y en a (et un peu après), jusqu'à ce qu'une
/// sortie de salon prenne le relais.
fn play_alone() {
    let mut stream = None;
    let broken = super::Broken::default();
    let mut last = Instant::now();
    loop {
        let (busy, taken) = {
            let p = PLAYER.lock().unwrap();
            (p.busy(), !p.sinks.is_empty())
        };
        if busy {
            last = Instant::now();
        }
        if taken || last.elapsed() > LINGER {
            return;
        }
        if broken.take() {
            stream = None;
        }
        if busy && stream.is_none() {
            let output = PLAYER.lock().unwrap().output.clone();
            let mut mono = vec![0f32; super::dsp::BLOCK];
            let fill = move |out: &mut [[f32; 2]]| {
                let mono = &mut mono[..out.len()];
                mono.fill(0.0);
                PLAYER.lock().unwrap().mix(ALONE, mono);
                for (o, m) in out.iter_mut().zip(mono.iter()) {
                    *o = [*m, *m];
                }
            };
            match super::open_output(output.as_deref(), fill, &broken) {
                Ok((s, _)) => stream = Some(s),
                Err(e) => {
                    warn!("sons d'interface : {e:#}");
                    PLAYER.lock().unwrap().playing = [None; VOICES];
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sons_courts_et_bornes() {
        for cue in ALL {
            let s = cue.samples();
            let secs = s.len() as f32 / RATE as f32;
            assert!((0.1..0.6).contains(&secs), "{cue:?} : {secs} s");
            let peak = s.iter().fold(0f32, |m, x| m.max(x.abs()));
            assert!((PEAK * 0.99..=PEAK + 1e-6).contains(&peak), "{cue:?} : crête {peak}");
            // Pas de clic au début ni à la fin.
            assert!(s[0].abs() < 0.01 && s[s.len() - 1].abs() < 0.01, "{cue:?}");
        }
    }

    #[test]
    fn sons_tous_differents() {
        for (i, a) in ALL.iter().enumerate() {
            for b in &ALL[i + 1..] {
                assert_ne!(a.samples(), b.samples(), "{a:?} et {b:?}");
            }
        }
    }

    #[test]
    fn sons_mixes_au_volume_jusqu_au_bout() {
        let mut p = Player::new();
        p.volume = 0.5;
        let samples = Cue::Join.samples();
        p.start(samples);
        let mut out = vec![0f32; 480];
        p.mix(ALONE, &mut out);
        assert!(out.iter().zip(samples).all(|(o, s)| (o - s * 0.5).abs() < 1e-6));
        // Il se termine, puis plus rien.
        let mut played = out.len();
        while p.busy() {
            out.fill(0.0);
            p.mix(ALONE, &mut out);
            played += out.len();
        }
        assert!(played >= samples.len() && played < samples.len() + 480);
        out.fill(0.0);
        p.mix(ALONE, &mut out);
        assert!(out.iter().all(|s| *s == 0.0));
    }

    #[test]
    fn une_seule_sortie_joue_les_sons() {
        let mut p = Player::new();
        p.sinks = vec![1, 2];
        p.start(Cue::Mention.samples());
        let mut out = vec![0f32; 480];
        // Ni la sortie à part, ni une sortie de salon plus ancienne.
        p.mix(ALONE, &mut out);
        p.mix(1, &mut out);
        assert!(out.iter().all(|s| *s == 0.0));
        p.mix(2, &mut out);
        assert!(out.iter().any(|s| *s != 0.0));
    }

    #[test]
    fn sons_superposes_restent_bornes() {
        let mut p = Player::new();
        p.volume = 1.0;
        for cue in [Cue::Join, Cue::Mention, Cue::Message, Cue::Mute] {
            p.start(cue.samples());
        }
        let mut out = vec![0.9f32; 4800];
        p.mix(ALONE, &mut out);
        assert!(out.iter().all(|s| s.abs() <= 1.0));
    }
}
