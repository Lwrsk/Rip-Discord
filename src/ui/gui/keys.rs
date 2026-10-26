//! Raccourcis clavier globaux : ils marchent même quand la fenêtre n'a pas le
//! focus (en jeu, par exemple). L'état des touches est lu ~120 fois par
//! seconde, seulement quand un raccourci peut servir (dans un serveur, ou
//! pendant l'enregistrement d'un raccourci) ; rien n'est intercepté, les
//! autres applis reçoivent les touches normalement.
//!
//! Les raccourcis partent directement vers la session réseau (pas via
//! l'interface, qui ne se redessine pas quand la fenêtre est réduite).

use std::{
    collections::BTreeSet,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use device_query::{DeviceQuery, DeviceState};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::Cmd;
use crate::core::identity::UserId;

/// Une touche du clavier ou un bouton de la souris.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Input {
    Key(String),
    Mouse(usize),
}

impl fmt::Display for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Input::Key(k) => f.write_str(key_label(k)),
            Input::Mouse(b) if *b == MIDDLE => f.write_str("Clic molette"),
            Input::Mouse(b) => write!(f, "Souris {b}"),
        }
    }
}

/// Bouton du milieu de la souris selon le système.
#[cfg(windows)]
const MIDDLE: usize = 3;
#[cfg(not(windows))]
const MIDDLE: usize = 2;

/// Boutons de souris utilisables (jamais les clics gauche et droit).
fn mouse_allowed(b: usize) -> bool {
    // Sous Windows, 4 et 5 sont les boutons latéraux ; ailleurs ce sont les
    // crans de la molette.
    b == MIDDLE || cfg!(windows) && (4..=5).contains(&b)
}

fn key_label(k: &str) -> &str {
    match k {
        "LControl" => "Ctrl gauche",
        "RControl" => "Ctrl droit",
        "LShift" => "Maj gauche",
        "RShift" => "Maj droite",
        "LAlt" => "Alt",
        "RAlt" => "Alt Gr",
        "LMeta" | "RMeta" | "Command" => "Windows",
        "Space" => "Espace",
        "Enter" => "Entrée",
        "Escape" => "Échap",
        "Backspace" => "Retour arrière",
        "Tab" => "Tab",
        "CapsLock" => "Verr. maj",
        "Insert" => "Inser",
        "Delete" => "Suppr",
        "Home" => "Début",
        "End" => "Fin",
        "PageUp" => "Page préc.",
        "PageDown" => "Page suiv.",
        "Up" => "Flèche haut",
        "Down" => "Flèche bas",
        "Left" => "Flèche gauche",
        "Right" => "Flèche droite",
        "Grave" => "²",
        k => k.strip_prefix("Key").or_else(|| k.strip_prefix("Numpad")).unwrap_or(k),
    }
}

/// Une combinaison : toutes ses touches enfoncées en même temps.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Combo(pub BTreeSet<Input>);

impl Combo {
    fn held(&self, pressed: &BTreeSet<Input>) -> bool {
        !self.0.is_empty() && self.0.is_subset(pressed)
    }
}

impl fmt::Display for Combo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Modificateurs d'abord : « Ctrl gauche + M ».
        let mut parts: Vec<&Input> = self.0.iter().collect();
        parts.sort_by_key(
            |i| !matches!(i, Input::Key(k) if k.contains("Control") || k.contains("Shift") || k.contains("Alt")),
        );
        let labels: Vec<String> = parts.iter().map(|i| i.to_string()).collect();
        f.write_str(&labels.join(" + "))
    }
}

/// Chuchoter à quelques personnes du salon tant que la touche est enfoncée.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Whisper {
    pub name: String,
    pub combo: Option<Combo>,
    pub users: Vec<UserId>,
    /// Pseudos au moment du choix (pour les afficher hors ligne).
    pub names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Bindings {
    pub mute: Option<Combo>,
    pub deafen: Option<Combo>,
    /// Appui-pour-parler.
    pub talk: Option<Combo>,
    pub whispers: Vec<Whisper>,
}

/// Raccourci en cours d'enregistrement.
#[derive(Debug, Clone, PartialEq)]
pub enum Recording {
    Off,
    /// Touches enfoncées jusqu'ici (on attend qu'elles soient relâchées).
    Listening(BTreeSet<Input>),
    Done(Combo),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Mute,
    Deafen,
    Talk,
    Whisper(usize),
}

struct Shared {
    bindings: Bindings,
    target: Option<mpsc::UnboundedSender<Cmd>>,
    recording: Recording,
    /// `None` tant que le fil n'a pas démarré ; faux si le système ne permet
    /// pas de lire le clavier (Wayland sans X, par exemple).
    available: Option<bool>,
}

impl Shared {
    /// Faut-il lire le clavier : un raccourci à enregistrer, ou une session
    /// et au moins un raccourci choisi.
    fn listening(&self) -> bool {
        let b = &self.bindings;
        let any =
            b.mute.is_some() || b.deafen.is_some() || b.talk.is_some() || b.whispers.iter().any(|w| w.combo.is_some());
        matches!(self.recording, Recording::Listening(_)) || (self.target.is_some() && any)
    }
}

/// Attente entre deux vérifications quand aucun raccourci ne peut servir.
const IDLE: Duration = Duration::from_millis(150);

#[derive(Clone)]
pub struct Hotkeys(Arc<Mutex<Shared>>);

impl Hotkeys {
    pub fn start(bindings: Bindings) -> Self {
        let keys = Self(Arc::new(Mutex::new(Shared {
            bindings,
            target: None,
            recording: Recording::Off,
            available: None,
        })));
        let shared = keys.0.clone();
        let _ = std::thread::Builder::new()
            .name("raccourcis".into())
            .spawn(move || watch(shared));
        keys
    }

    pub fn bindings(&self) -> Bindings {
        self.0.lock().unwrap().bindings.clone()
    }

    pub fn set_bindings(&self, b: Bindings) {
        self.0.lock().unwrap().bindings = b;
    }

    /// Session qui reçoit les raccourcis (`None` : aucune).
    pub fn set_target(&self, target: Option<mpsc::UnboundedSender<Cmd>>) {
        self.0.lock().unwrap().target = target;
    }

    pub fn available(&self) -> Option<bool> {
        self.0.lock().unwrap().available
    }

    pub fn record(&self) {
        self.0.lock().unwrap().recording = Recording::Listening(BTreeSet::new());
    }

    pub fn cancel_recording(&self) {
        self.0.lock().unwrap().recording = Recording::Off;
    }

    pub fn recording(&self) -> Recording {
        self.0.lock().unwrap().recording.clone()
    }

    /// Récupère le raccourci enregistré, s'il est prêt.
    pub fn take_recorded(&self) -> Option<Combo> {
        let mut s = self.0.lock().unwrap();
        match std::mem::replace(&mut s.recording, Recording::Off) {
            Recording::Done(c) => Some(c),
            other => {
                s.recording = other;
                None
            }
        }
    }
}

fn pressed(device: &DeviceState) -> BTreeSet<Input> {
    let mut set: BTreeSet<Input> = device
        .get_keys()
        .into_iter()
        .map(|k| Input::Key(format!("{k:?}")))
        .collect();
    let mouse = device.get_mouse();
    for (b, down) in mouse.button_pressed.iter().enumerate() {
        if *down && mouse_allowed(b) {
            set.insert(Input::Mouse(b));
        }
    }
    set
}

fn watch(shared: Arc<Mutex<Shared>>) {
    let Some(device) = DeviceState::checked_new() else {
        tracing::warn!("raccourcis globaux indisponibles (pas d'accès au clavier)");
        shared.lock().unwrap().available = Some(false);
        return;
    };
    shared.lock().unwrap().available = Some(true);
    let mut active: Vec<Action> = Vec::new();
    loop {
        if !shared.lock().unwrap().listening() {
            // Rien à surveiller : le clavier n'est pas lu (presque aucun
            // processeur), on revérifie de temps en temps.
            active.clear();
            std::thread::sleep(IDLE);
            continue;
        }
        std::thread::sleep(Duration::from_millis(8));
        let now = pressed(&device);
        let mut s = shared.lock().unwrap();
        if let Recording::Listening(acc) = &mut s.recording {
            acc.extend(now.iter().cloned());
            if now.is_empty() && !acc.is_empty() {
                let combo = Combo(std::mem::take(acc));
                s.recording = Recording::Done(combo);
            }
            // Pas d'action pendant l'enregistrement d'un raccourci.
            active.clear();
            continue;
        }
        let b = &s.bindings;
        let mut held = Vec::new();
        let mut check = |combo: &Option<Combo>, action| {
            if combo.as_ref().is_some_and(|c| c.held(&now)) {
                held.push(action);
            }
        };
        check(&b.mute, Action::Mute);
        check(&b.deafen, Action::Deafen);
        check(&b.talk, Action::Talk);
        for (i, w) in b.whispers.iter().enumerate() {
            check(&w.combo, Action::Whisper(i));
        }
        let Some(target) = s.target.clone() else {
            active = held;
            continue;
        };
        for a in held.iter().filter(|a| !active.contains(a)) {
            let cmd = match a {
                Action::Mute => Cmd::ToggleMute,
                Action::Deafen => Cmd::ToggleDeafen,
                Action::Talk => Cmd::Talk {
                    held: true,
                    whisper: None,
                },
                Action::Whisper(i) => Cmd::Talk {
                    held: true,
                    whisper: Some(b.whispers[*i].users.clone()),
                },
            };
            let _ = target.send(cmd);
        }
        let released = active
            .iter()
            .any(|a| matches!(a, Action::Talk | Action::Whisper(_)) && !held.contains(a));
        let still_talking = held.iter().any(|a| matches!(a, Action::Talk | Action::Whisper(_)));
        if released && !still_talking {
            let _ = target.send(Cmd::Talk {
                held: false,
                whisper: None,
            });
        }
        active = held;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le clavier n'est lu que si un raccourci peut servir.
    #[test]
    fn clavier_lu_seulement_si_utile() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut s = Shared {
            bindings: Bindings::default(),
            target: None,
            recording: Recording::Off,
            available: None,
        };
        assert!(!s.listening());
        s.target = Some(tx);
        assert!(!s.listening(), "dans un serveur, mais aucun raccourci choisi");
        s.bindings.talk = Some(Combo([Input::Key("V".into())].into()));
        assert!(s.listening());
        s.target = None;
        assert!(!s.listening(), "hors d'un serveur");
        s.recording = Recording::Listening(BTreeSet::new());
        assert!(s.listening(), "enregistrement d'un raccourci");
    }

    #[test]
    fn combinaison_et_libelle() {
        let ctrl_m = Combo([Input::Key("M".into()), Input::Key("LControl".into())].into());
        assert_eq!(ctrl_m.to_string(), "Ctrl gauche + M");
        let mut now = BTreeSet::from([Input::Key("M".into())]);
        assert!(!ctrl_m.held(&now));
        now.insert(Input::Key("LControl".into()));
        now.insert(Input::Key("A".into()));
        assert!(ctrl_m.held(&now));
        assert!(!Combo::default().held(&now));
        assert_eq!(Input::Key("Key4".into()).to_string(), "4");
    }
}
