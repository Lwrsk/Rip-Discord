//! Rejoindre un serveur à partir d'un code d'invitation : adresse directe
//! d'abord, puis annuaire décentralisé et perforation UDP.

use std::{net::SocketAddr, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use quinn::Endpoint;
use tokio::{sync::mpsc, time::timeout};
use tracing::{debug, info};

use super::{Client, Event};
use crate::core::{
    card::{AccessKey, now_ms},
    directory::{Directory, HostRecord, Knock, Slots},
    identity::Identity,
    invite::Invite,
    nat, tls,
};

/// Temps laissé à une adresse directe avant de passer à l'annuaire.
const DIRECT_TIMEOUT: Duration = Duration::from_secs(4);
/// Temps laissé à l'adresse locale de l'hôte (même box) : la réponse vient
/// en quelques millisecondes quand elle est bonne.
const LAN_TIMEOUT: Duration = Duration::from_millis(1500);
/// Temps total laissé à la perforation : l'hôte doit lire notre frappe dans
/// l'annuaire puis nous envoyer des paquets.
const PUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Reçoit les étapes de la connexion, à afficher (« Hôte trouvé… »).
pub type Progress<'a> = &'a (dyn Fn(String) + Send + Sync);

pub async fn join(
    invite: &Invite,
    identity: &Identity,
    name: &str,
) -> Result<(Client, mpsc::UnboundedReceiver<Event>)> {
    let ipv6 = invite.direct.is_some_and(|a| a.is_ipv6());
    let (endpoint, probe) = endpoint(ipv6)?;
    let dir = async {
        Directory::start()
            .await
            .inspect_err(|e| debug!("annuaire : {e:#}"))
            .ok()
    };
    join_on(endpoint, probe, invite, identity, name, dir, &|step| info!("{step}")).await
}

/// Comme [`join`], depuis un endpoint existant (celui de la session). `dir`
/// donne l'annuaire (déjà démarré, de préférence) : il est interrogé en même
/// temps qu'on essaie l'adresse directe du code.
pub async fn join_on(
    endpoint: Endpoint,
    probe: nat::Probe,
    invite: &Invite,
    identity: &Identity,
    name: &str,
    dir: impl Future<Output = Option<Directory>> + Send + 'static,
    progress: Progress<'_>,
) -> Result<(Client, mpsc::UnboundedReceiver<Event>)> {
    let mut last_err = None;

    // L'annuaire répond en quelques secondes : on le consulte tout de suite,
    // sans attendre de savoir si l'adresse directe répond.
    let lookup = invite.key.map(|key| {
        tokio::spawn(async move {
            let dir = dir.await?;
            let found = dir.lookup(&Slots::new(key)).await;
            Some((dir, found))
        })
    });

    if let Some(addr) = invite.direct {
        progress(format!("Connexion directe à l'hôte ({addr})…"));
        let wait = if invite.key.is_some() {
            DIRECT_TIMEOUT
        } else {
            Duration::from_secs(10)
        };
        let pin = invite.cert.map(|c| c.to_vec());
        match attempt(&endpoint, &probe, addr, identity, name, invite.key, pin, wait).await {
            Err(e) if is_refused(&e) => return Err(e),
            Ok(ok) => {
                if let Some(l) = lookup {
                    l.abort();
                }
                return Ok(ok);
            }
            Err(e) => {
                debug!("adresse directe {addr} : {e:#}");
                last_err = Some(e);
            }
        }
    }

    let (Some(key), Some(lookup)) = (invite.key, lookup) else {
        return Err(last_err.unwrap_or_else(|| anyhow!("code d'invitation vide"))).context(
            "L'hôte ne répond pas. Vérifie le code ; si l'hôte est en « réseau local seulement », \
             vous devez être sur le même Wi-Fi.",
        );
    };
    progress("Recherche de l'hôte dans l'annuaire décentralisé…".into());
    let Some((dir, found)) = lookup.await.ok().flatten() else {
        bail!(
            "Annuaire décentralisé injoignable : vérifie ta connexion à Internet (ou un pare-feu qui bloquerait l'UDP)."
        );
    };
    let host = match found {
        Some((_, host)) if host.is_fresh() => host,
        _ => bail!(
            "Personne n'héberge ce serveur en ce moment. Si tu en es déjà membre, clique « Ouvrir » \
             dans « Mes serveurs » : ton PC l'hébergera."
        ),
    };
    join_host(
        &dir,
        endpoint,
        probe,
        key,
        &host,
        identity,
        name,
        PUNCH_TIMEOUT,
        progress,
    )
    .await
}

/// Rejoint l'hôte trouvé dans l'annuaire, en perçant les box au besoin.
/// `patience` : temps laissé à la perforation avant d'abandonner.
#[allow(clippy::too_many_arguments)]
pub async fn join_host(
    dir: &Directory,
    endpoint: Endpoint,
    probe: nat::Probe,
    key: AccessKey,
    host: &HostRecord,
    identity: &Identity,
    name: &str,
    patience: Duration,
    progress: Progress<'_>,
) -> Result<(Client, mpsc::UnboundedReceiver<Event>)> {
    let pin = Some(host.cert.to_vec());
    let mut last_err = None;
    progress("Hôte trouvé : connexion…".into());

    // Même box : l'adresse locale passe sans perforation. On l'essaie pendant
    // qu'on demande notre adresse publique, plutôt qu'après.
    let mut stun = probe.clone();
    let discover = async {
        match stun.discover().await {
            Ok(m) => Some(m.addr()),
            Err(e) => {
                debug!("STUN : {e:#}");
                None
            }
        }
    };
    let lan = async {
        let lan = host.addr.lan.filter(|lan| *lan != host.addr.public)?;
        Some(
            attempt(
                &endpoint,
                &probe,
                lan,
                identity,
                name,
                Some(key),
                pin.clone(),
                LAN_TIMEOUT,
            )
            .await,
        )
    };
    let (me, lan) = tokio::join!(discover, lan);
    match lan {
        Some(Ok(ok)) => return Ok(ok),
        Some(Err(e)) if is_refused(&e) => return Err(e),
        Some(Err(e)) => last_err = Some(e),
        None => {}
    }

    // Peut-être déjà joignable (port ouvert, box tolérante) ; sinon on frappe
    // et on réessaie tant que l'hôte n'a pas percé.
    let public = host.addr.public;
    let knock = {
        let dir = dir.clone();
        let knock = Knock {
            public: me,
            lan: endpoint.local_addr().ok().and_then(|a| lan_of(a.port())),
            at_ms: now_ms(),
        };
        tokio::spawn(async move { dir.knock(&Slots::new(key), &knock).await })
    };
    info!("connexion à l'hôte {public} (perforation via l'annuaire)");
    progress("Hôte trouvé : ouverture d'un passage entre vos deux box…".into());
    let deadline = tokio::time::Instant::now() + patience;
    while tokio::time::Instant::now() < deadline {
        let started = tokio::time::Instant::now();
        let wait = Duration::from_secs(4)
            .min(deadline - started)
            .max(Duration::from_secs(1));
        match attempt(&endpoint, &probe, public, identity, name, Some(key), pin.clone(), wait).await {
            Ok(ok) => {
                knock.abort();
                return Ok(ok);
            }
            Err(e) if is_refused(&e) => {
                knock.abort();
                return Err(e);
            }
            Err(e) => {
                debug!("{public} : {e:#}");
                last_err = Some(e);
                // Refus immédiat (paquet rejeté) : on laisse à l'hôte le temps de percer.
                tokio::time::sleep_until(started + Duration::from_millis(500)).await;
            }
        }
    }
    if let Ok(Ok(Err(e))) = timeout(Duration::from_millis(10), knock).await {
        debug!("frappe : {e:#}");
    }
    Err(last_err.unwrap_or_else(|| anyhow!("hôte injoignable"))).context(
        "L'hôte a été trouvé mais ne répond pas : il vient peut-être de partir, ou vos deux box \
         refusent la connexion directe (NAT strict). Réessaie dans un instant ; si ça dure, l'un \
         de vous peut ouvrir le port à la main (voir le README).",
    )
}

fn lan_of(port: u16) -> Option<SocketAddr> {
    crate::server::upnp::lan_ip().map(|ip| SocketAddr::new(ip, port))
}

/// Endpoint client capable de découvrir son adresse publique.
pub fn endpoint(ipv6: bool) -> Result<(Endpoint, nat::Probe)> {
    let bind: SocketAddr = if ipv6 { "[::]:0" } else { "0.0.0.0:0" }.parse()?;
    let (mut endpoint, probe) = nat::endpoint(bind, None)?;
    endpoint.set_default_client_config(tls::client_config()?);
    Ok((endpoint, probe))
}

fn is_refused(e: &anyhow::Error) -> bool {
    e.downcast_ref::<super::Refused>().is_some()
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    endpoint: &Endpoint,
    probe: &nat::Probe,
    addr: SocketAddr,
    identity: &Identity,
    name: &str,
    key: Option<AccessKey>,
    pin: Option<Vec<u8>>,
    wait: Duration,
) -> Result<(Client, mpsc::UnboundedReceiver<Event>)> {
    let opts = super::Options {
        key,
        pin,
        ..Default::default()
    };
    let (mut client, events) = timeout(
        wait,
        Client::connect_with(endpoint.clone(), addr, identity, name, &opts),
    )
    .await
    .map_err(|_| anyhow!("pas de réponse de {addr}"))??;
    client.probe = Some(probe.clone());
    Ok((client, events))
}
