//! Relève d'hôte, côté membre : l'hôte est parti, on suit l'ordre de
//! succession qu'il avait diffusé (voir [`crate::server::migration`]).

use std::{net::SocketAddr, time::Duration};

use anyhow::{Result, anyhow};
use quinn::{Connection, Endpoint};
use tokio::{sync::mpsc, task::JoinSet, time::timeout};
use tracing::{debug, info};

use super::{Client, Event, Options};
use crate::core::{
    card::AccessKey,
    identity::Identity,
    protocol::{CertFingerprint, Contact, PeerId},
    tls,
};

/// Temps laissé à chaque successeur pour répondre avant de passer au suivant.
/// Couvre l'écart entre le moment où il remarque la panne et celui où on la
/// remarque nous-mêmes.
pub const RANK_WINDOW: Duration = Duration::from_secs(4);
/// Hôte muet : on vérifie d'abord brièvement que ce n'est pas notre propre
/// connexion qui a sauté (l'hôte serait alors toujours là pour les autres).
const RECHECK: Duration = Duration::from_millis(1000);

/// Ce qu'on sait au moment où l'hôte disparaît.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Clé d'accès du serveur (la plus récente connue).
    pub key: AccessKey,
    pub old_host: SocketAddr,
    /// Certificat de l'ancien hôte : s'il répond encore, c'est bien lui.
    pub old_cert: Option<CertFingerprint>,
    /// Derniers contacts reçus, dans l'ordre de succession (ceux qui ne
    /// peuvent pas prendre la relève, comme l'ancien hôte, sont ignorés ici
    /// mais le nouvel hôte perce aussi sa box vers eux).
    pub contacts: Vec<Contact>,
    /// L'hôte a prévenu de son départ : inutile de vérifier s'il est toujours là.
    pub graceful: bool,
    /// Mon numéro chez l'ancien hôte.
    pub me: PeerId,
}

impl Plan {
    /// Successeur qui passera avant moi, le cas échéant (pour l'afficher).
    pub fn heirs(&self) -> impl Iterator<Item = &Contact> {
        self.contacts.iter().filter(|c| c.heir)
    }

    pub fn first_heir(&self) -> Option<&Contact> {
        self.heirs().next()
    }

    pub fn my_rank(&self) -> Option<usize> {
        self.heirs().position(|h| h.peer == self.me)
    }
}

pub enum Outcome {
    /// Connecté au nouvel hôte (ou à l'ancien, qui était toujours là).
    Joined(Box<Client>, mpsc::UnboundedReceiver<Event>),
    /// C'est à moi d'héberger.
    TakeOver,
    /// Personne n'a répondu : il faut passer par l'annuaire.
    Lost,
}

/// Suit l'ordre de succession depuis `endpoint` (celui qui parlait à l'ancien
/// hôte : son port est déjà ouvert dans la box). `progress` reçoit le
/// successeur essayé.
pub async fn follow(
    plan: &Plan,
    endpoint: &Endpoint,
    identity: &Identity,
    name: &str,
    mut progress: impl FnMut(&Contact),
) -> Outcome {
    let back = Options {
        key: Some(plan.key),
        pin: plan.old_cert.map(|c| c.to_vec()),
        resume: Some(plan.me),
    };
    for (rank, heir) in plan.heirs().enumerate() {
        if heir.peer == plan.me {
            if !plan.graceful
                && let Ok(Ok(joined)) = timeout(
                    RECHECK,
                    Client::connect_with(endpoint.clone(), plan.old_host, identity, name, &back),
                )
                .await
            {
                info!("l'hôte est toujours là : c'est ma connexion qui avait sauté");
                return Outcome::Joined(Box::new(joined.0), joined.1);
            }
            return Outcome::TakeOver;
        }
        progress(heir);
        let opts = Options {
            key: Some(plan.key),
            pin: Some(heir.cert.to_vec()),
            resume: Some(plan.me),
        };
        let to_heir = connect_any(endpoint, &heir.addrs, heir.cert, identity, name, &opts);
        let attempt = async {
            // Premier successeur : l'ancien hôte est peut-être toujours là.
            if rank == 0 && !plan.graceful {
                let old = async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Client::connect_with(endpoint.clone(), plan.old_host, identity, name, &back).await
                };
                tokio::select! {
                    Ok(joined) = to_heir => Ok(joined),
                    Ok(joined) = old => Ok(joined),
                    else => Err(anyhow!("ni le successeur ni l'ancien hôte ne répondent")),
                }
            } else {
                to_heir.await
            }
        };
        match timeout(RANK_WINDOW, attempt).await {
            Ok(Ok((client, events))) => return Outcome::Joined(Box::new(client), events),
            Ok(Err(e)) => debug!("successeur {} : {e:#}", heir.peer),
            Err(_) => debug!("successeur {} : pas de réponse", heir.peer),
        }
    }
    if !plan.graceful
        && let Ok(Ok((client, events))) = timeout(
            RECHECK,
            Client::connect_with(endpoint.clone(), plan.old_host, identity, name, &back),
        )
        .await
    {
        return Outcome::Joined(Box::new(client), events);
    }
    Outcome::Lost
}

/// Se connecte au successeur par la première de ses adresses qui répond
/// (réseau local, adresse publique…), en vérifiant son certificat. Les
/// poignées de main partent en parallèle, mais on ne se présente qu'une fois.
async fn connect_any(
    endpoint: &Endpoint,
    addrs: &[SocketAddr],
    cert: CertFingerprint,
    identity: &Identity,
    name: &str,
    opts: &Options,
) -> Result<(Client, mpsc::UnboundedReceiver<Event>)> {
    let conn = first_handshake(endpoint, addrs, tls::client_config_pinned(&cert)?).await?;
    Client::over(endpoint.clone(), conn, identity, name, opts).await
}

pub(crate) async fn first_handshake(
    endpoint: &Endpoint,
    addrs: &[SocketAddr],
    config: quinn::ClientConfig,
) -> Result<Connection> {
    let mut tries = JoinSet::new();
    for (i, addr) in addrs.iter().copied().enumerate() {
        let (endpoint, config) = (endpoint.clone(), config.clone());
        tries.spawn(async move {
            // Un léger décalage favorise l'adresse locale, souvent la meilleure.
            tokio::time::sleep(Duration::from_millis(150 * i as u64)).await;
            // La box du successeur s'ouvre quand il nous envoie ses premiers
            // paquets : on réessaie tant qu'il n'a pas percé.
            loop {
                let attempt = match endpoint.connect_with(config.clone(), addr, "localhost") {
                    Ok(connecting) => connecting.await.map_err(anyhow::Error::from),
                    Err(e) => Err(e.into()),
                };
                match attempt {
                    Ok(conn) => return Ok(conn),
                    Err(e) if is_refused(&e) => tokio::time::sleep(Duration::from_millis(300)).await,
                    Err(e) => return Err(e),
                }
            }
        });
    }
    let mut last = anyhow!("aucune adresse connue pour le successeur");
    while let Some(done) = tries.join_next().await {
        match done {
            Ok(Ok(conn)) => {
                // Les autres poignées de main, si elles aboutissent, sont refermées.
                tokio::spawn(async move {
                    while let Some(done) = tries.join_next().await {
                        if let Ok(Ok(extra)) = done {
                            extra.close(0u32.into(), b"doublon");
                        }
                    }
                });
                return Ok(conn);
            }
            Ok(Err(e)) => last = e,
            Err(e) => last = e.into(),
        }
    }
    Err(last)
}

/// Le successeur n'héberge pas encore (paquet refusé) : réessayer vaut le coup.
fn is_refused(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<quinn::ConnectionError>(),
            Some(quinn::ConnectionError::ConnectionClosed(_) | quinn::ConnectionError::Reset)
        )
    })
}
