//! Annonce de l'hôte dans l'annuaire décentralisé (voir
//! [`crate::core::directory`]) et perforation de la box vers les arrivants.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use quinn::Endpoint;
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, info, warn};

use super::Host;
use crate::core::{
    card::{AccessKey, now_ms},
    directory::{Directory, HostAddr, HostRecord, KNOCK_SLOTS, REFRESH, Slots},
    identity::UserId,
    nat::{Mapping, Probe},
    protocol::CertFingerprint,
    tls,
};

/// Clé d'accès en vigueur, relue à chaque rafraîchissement : si un admin
/// change le code, l'annonce passe dans les cases de la nouvelle clé.
pub type KeySource = Arc<dyn Fn() -> AccessKey + Send + Sync>;

/// À quelle fréquence l'hôte regarde si quelqu'un frappe à la porte.
const KNOCK_POLL: Duration = Duration::from_secs(2);

pub enum Hosting {
    /// Publié. Tant que l'annonce vit, l'annuaire est rafraîchi et la box
    /// percée vers chaque arrivant.
    Announced(Announcement),
    /// Un autre membre héberge déjà ce serveur : il faut le rejoindre.
    AlreadyHosted(HostRecord),
}

pub struct Announcement {
    pub mapping: Mapping,
    /// Un autre membre s'est annoncé hôte après nous (relève pendant que notre
    /// connexion avait sauté, par exemple) : il faudra peut-être lui céder la place.
    pub superseded: watch::Receiver<Option<HostRecord>>,
    stop: oneshot::Sender<bool>,
    task: JoinHandle<()>,
}

impl Announcement {
    /// Retire l'hôte de l'annuaire : le prochain membre qui ouvre le serveur
    /// saura tout de suite qu'il doit l'héberger. `alone` : personne n'était
    /// connecté, donc personne ne prend la relève ; on retire sans vérifier,
    /// pour que ce soit fait même quand l'application se ferme.
    pub async fn stop(self, alone: bool) {
        let _ = self.stop.send(alone);
        let _ = timeout(Duration::from_secs(20), self.task).await;
    }
}

/// Publie `host` comme hôte de son serveur, sauf si un autre membre
/// l'héberge déjà.
pub async fn announce(dir: &Directory, host: &Host, me: UserId) -> Result<Hosting> {
    let handle = host.handle();
    announce_on(
        dir,
        host.endpoint(),
        host.probe(),
        host.fingerprint(),
        Arc::new(move || handle.access_key()),
        me,
        false,
    )
    .await
}

/// Comme [`announce`], pour un hôte qui tourne sur `endpoint`. `takeover` :
/// on prend la relève d'un hôte disparu, dont l'annonce paraît encore fraîche
/// (on la remplace sans attendre qu'elle expire).
pub async fn announce_on(
    dir: &Directory,
    endpoint: Endpoint,
    mut probe: Probe,
    cert: CertFingerprint,
    key: KeySource,
    me: UserId,
    takeover: bool,
) -> Result<Hosting> {
    let mapping = probe
        .discover()
        .await
        .context("adresse publique introuvable (pas d'accès à Internet ?)")?;
    if let Mapping::Symmetric(_) = mapping {
        warn!("box à NAT symétrique : les membres auront du mal à entrer sans port ouvert");
    }
    let port = endpoint.local_addr()?.port();
    let addr = HostAddr {
        public: mapping.addr(),
        lan: super::upnp::lan_ip().map(|ip| SocketAddr::new(ip, port)),
    };
    let mut current = key();
    let mut slots = Slots::new(current);

    let previous = dir.lookup(&slots).await;
    if !takeover
        && let Some((_, other)) = previous
        && other.user != me
        && other.is_fresh()
    {
        return Ok(Hosting::AlreadyHosted(other));
    }
    let record = |at_ms| HostRecord {
        user: me,
        addr,
        cert,
        at_ms,
    };
    let mut published = now_ms();
    let mut seq = dir
        .publish(&slots, previous.map_or(0, |p| p.0), &record(published))
        .await?;

    // Deux membres ont pu ouvrir le serveur au même instant : le plus petit
    // identifiant garde la main.
    if !takeover {
        tokio::time::sleep(Duration::from_millis(800)).await;
        if let Some((latest, other)) = dir.lookup(&slots).await
            && other.user != me
            && other.is_fresh()
        {
            if other.user < me {
                return Ok(Hosting::AlreadyHosted(other));
            }
            published = now_ms();
            seq = dir.publish(&slots, latest.max(seq), &record(published)).await?;
        }
    }
    info!("annoncé dans l'annuaire : joignable à {}", addr.public);

    let (stop, mut stopped) = oneshot::channel();
    let (rival_tx, superseded) = watch::channel(None);
    let dir = dir.clone();
    let task = tokio::spawn(async move {
        let mut addr = addr;
        let mut seen = [0; KNOCK_SLOTS as usize];
        // Les frappes déjà présentes sont anciennes : on les ignore.
        dir.knocks(&slots, &mut seen).await;
        let mut poll = tokio::time::interval(KNOCK_POLL);
        let mut refresh = tokio::time::interval(REFRESH);
        refresh.reset();
        loop {
            tokio::select! {
                _ = poll.tick() => {
                    for knock in dir.knocks(&slots, &mut seen).await {
                        for to in knock.public.into_iter().chain(knock.lan) {
                            punch(&endpoint, to);
                        }
                    }
                }
                _ = refresh.tick() => {
                    // Nouveau code : l'ancienne case est vidée, on passe à la nouvelle.
                    let now = key();
                    if now != current {
                        info!("annuaire : nouvelle clé d'accès, l'annonce change de case");
                        let gone = HostRecord { user: me, addr, cert, at_ms: 0 };
                        if let Err(e) = dir.publish(&slots, seq, &gone).await {
                            debug!("annuaire : retrait de l'ancienne case : {e:#}");
                        }
                        current = now;
                        slots = Slots::new(current);
                        seq = 0;
                        seen = [0; KNOCK_SLOTS as usize];
                        dir.knocks(&slots, &mut seen).await;
                    }
                    // Interroger STUN garde aussi l'ouverture de la box vivante.
                    match probe.discover().await {
                        Ok(m) if m.addr() != addr.public => {
                            info!("nouvelle adresse publique : {}", m.addr());
                            addr.public = m.addr();
                        }
                        Ok(_) => {}
                        Err(e) => debug!("STUN : {e:#}"),
                    }
                    // Quelqu'un s'est annoncé après nous : on le signale.
                    if let Some((latest, other)) = dir.lookup(&slots).await
                        && other.user != me
                        && other.is_fresh()
                        && other.at_ms > published
                    {
                        info!("un autre membre s'annonce hôte de ce serveur");
                        seq = seq.max(latest);
                        rival_tx.send_replace(Some(other));
                    }
                    published = now_ms();
                    let record = HostRecord { user: me, addr, cert, at_ms: published };
                    match dir.publish(&slots, seq, &record).await {
                        Ok(s) => seq = s,
                        Err(e) => debug!("annuaire : {e:#}"),
                    }
                }
                alone = &mut stopped => {
                    // Un successeur a peut-être déjà pris notre place (ou je
                    // ré-héberge déjà ailleurs, avec un autre certificat) : on
                    // n'efface que notre propre annonce. Vérification brève :
                    // sans réponse, on retire quand même la nôtre.
                    if !alone.unwrap_or(false)
                        && let Ok(Some((latest, other))) = timeout(Duration::from_secs(3), dir.lookup(&slots)).await
                        && (other.user != me || other.cert != cert)
                    {
                        debug!("annuaire : annonce déjà reprise (n° {latest})");
                        return;
                    }
                    let gone = HostRecord { user: me, addr, cert, at_ms: 0 };
                    if let Err(e) = dir.publish(&slots, seq, &gone).await {
                        warn!("annuaire : retrait impossible : {e:#}");
                    }
                    return;
                }
            }
        }
    });
    Ok(Hosting::Announced(Announcement {
        mapping,
        superseded,
        stop,
        task,
    }))
}

/// Envoie quelques paquets vers `addr` depuis le port du serveur : la box de
/// l'hôte laisse ensuite passer ceux du membre. Une tentative de connexion
/// QUIC fait l'affaire ; elle n'a pas besoin d'aboutir.
pub fn punch(endpoint: &Endpoint, addr: SocketAddr) {
    punch_for(endpoint, addr, 3);
}

/// Comme [`punch`], en insistant `rounds` fois (toutes les 700 ms).
pub fn punch_for(endpoint: &Endpoint, addr: SocketAddr, rounds: usize) {
    debug!("perforation vers {addr}");
    let endpoint = endpoint.clone();
    tokio::spawn(async move {
        for _ in 0..rounds {
            if let Ok(cfg) = tls::client_config()
                && let Ok(connecting) = endpoint.connect_with(cfg, addr, "punch")
            {
                tokio::spawn(async move {
                    let _ = timeout(Duration::from_secs(2), connecting).await;
                });
            }
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
    });
}
