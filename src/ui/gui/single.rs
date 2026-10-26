//! Une seule fenêtre par dossier de données : un deuxième lancement (double
//! clic sur le raccourci, lien d'invitation) ramène la fenêtre déjà ouverte,
//! lui passe son code d'invitation, puis s'arrête.
//!
//! La première instance écoute sur un port local (127.0.0.1 seulement) tiré
//! du dossier de données : deux dossiers différents (`RIP_DISCORD_DATA`)
//! restent deux apps indépendantes.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::mpsc as std_mpsc,
    time::Duration,
};

use eframe::egui;

/// Ce qu'envoie le deuxième lancement, suivi de son code (ou d'une ligne vide).
const HELLO: &str = "rip-discord : montre-toi";
const OK: &str = "ok";

pub enum Claim {
    /// Première fenêtre : écoute les suivantes (si le port a pu être pris).
    First(Option<TcpListener>),
    /// Une fenêtre est déjà ouverte et a été prévenue.
    AlreadyOpen,
}

fn port(root: &Path) -> u16 {
    // FNV-1a : stable d'une version de Rust à l'autre, contrairement au
    // hachage de la bibliothèque standard.
    let mut h: u32 = 0x811c_9dc5;
    for b in root.to_string_lossy().bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    40_000 + (h % 20_000) as u16
}

pub fn claim(root: &Path, code: Option<&str>) -> Claim {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port(root)));
    match TcpListener::bind(addr) {
        Ok(listener) => Claim::First(Some(listener)),
        Err(_) => match tell(addr, code) {
            Ok(()) => Claim::AlreadyOpen,
            Err(e) => {
                // Port pris par autre chose : on s'ouvre quand même.
                tracing::warn!("fenêtre déjà ouverte ? {e}");
                Claim::First(None)
            }
        },
    }
}

fn tell(addr: SocketAddr, code: Option<&str>) -> std::io::Result<()> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{HELLO}\n{}", code.unwrap_or_default().trim())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    if answer.trim() == OK {
        Ok(())
    } else {
        Err(std::io::Error::other("réponse inattendue"))
    }
}

/// Écoute les lancements suivants : chacun ramène la fenêtre au premier plan,
/// et son code d'invitation (s'il en a un) arrive dans le récepteur renvoyé.
pub fn listen(listener: Option<TcpListener>, ctx: egui::Context) -> std_mpsc::Receiver<String> {
    let (tx, rx) = std_mpsc::channel();
    let Some(listener) = listener else {
        return rx;
    };
    let spawned = std::thread::Builder::new().name("une-fenetre".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let Ok(code) = answer(stream) else {
                continue;
            };
            if !code.is_empty() {
                let _ = tx.send(code);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("une seule fenêtre : {e}");
    }
    rx
}

fn answer(mut stream: TcpStream) -> std::io::Result<String> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut lines = BufReader::new(stream.try_clone()?).lines();
    if lines.next().transpose()?.as_deref() != Some(HELLO) {
        return Err(std::io::Error::other("inconnu"));
    }
    let code = lines.next().transpose()?.unwrap_or_default();
    writeln!(stream, "{OK}")?;
    Ok(code.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deuxieme_lancement_ramene_le_premier() {
        let root = std::env::temp_dir().join(format!("rip-une-fenetre-{}", std::process::id()));
        let Claim::First(listener) = claim(&root, None) else {
            panic!("personne n'était ouvert");
        };
        assert!(listener.is_some());
        let codes = listen(listener, egui::Context::default());
        assert!(matches!(claim(&root, Some(" ABCD-EFGH ")), Claim::AlreadyOpen));
        assert_eq!(codes.recv_timeout(Duration::from_secs(2)).unwrap(), "ABCD-EFGH");
        assert!(matches!(claim(&root, None), Claim::AlreadyOpen));
        assert!(
            codes.recv_timeout(Duration::from_millis(200)).is_err(),
            "pas de code : rien à rejoindre"
        );

        // Un autre dossier de données : une autre app.
        assert!(matches!(claim(&root.join("autre"), None), Claim::First(Some(_))));
    }
}
