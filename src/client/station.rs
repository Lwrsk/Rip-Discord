//! Le port QUIC d'un membre, gardé toute la session : il parle à l'hôte du
//! serveur, reçoit les connexions directes des autres participants du salon
//! vocal (voir [`super::mesh`]), et héberge le serveur si l'on prend la relève.
//!
//! Comme c'est toujours le même port, la box le garde ouvert vers tous ceux
//! à qui on a déjà parlé : la relève et le vocal direct en profitent.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use quinn::{Connection, Endpoint};
use tokio::sync::mpsc;
use tracing::debug;

use crate::core::{
    ALPN, MESH_ALPN,
    nat::{self, Probe},
    tls::{self, HostCert},
};

type Route = Arc<Mutex<Option<mpsc::Sender<Connection>>>>;

#[derive(Clone)]
pub struct Station {
    pub endpoint: Endpoint,
    pub probe: Probe,
    /// Certificat présenté aux autres membres (vocal direct, relève).
    pub cert: HostCert,
    host: Route,
    mesh: Route,
}

/// Connexions ouvertes en même temps sur le port, au plus (membres du
/// serveur hébergé et vocal direct compris).
const MAX_CONNECTIONS: usize = 256;

impl Station {
    /// Ouvre un port au hasard (IPv4).
    pub fn open() -> Result<Self> {
        Self::bind(([0, 0, 0, 0], 0).into())
    }

    pub fn bind(addr: SocketAddr) -> Result<Self> {
        let cert = HostCert::generate()?;
        let (mut endpoint, probe) = nat::endpoint(addr, Some(cert.config.clone()))?;
        endpoint.set_default_client_config(tls::client_config()?);
        let station = Self {
            endpoint,
            probe,
            cert,
            host: Route::default(),
            mesh: Route::default(),
        };
        tokio::spawn(station.clone().dispatch());
        Ok(station)
    }

    /// Les connexions de membres au serveur arrivent désormais ici (on héberge).
    pub fn host_route(&self) -> mpsc::Receiver<Connection> {
        let (tx, rx) = mpsc::channel(32);
        *self.host.lock().unwrap() = Some(tx);
        rx
    }

    /// On n'héberge plus : les connexions au serveur sont refusées.
    pub fn stop_hosting(&self) {
        *self.host.lock().unwrap() = None;
    }

    /// Les connexions directes d'autres membres arrivent ici.
    pub fn mesh_route(&self) -> mpsc::Receiver<Connection> {
        let (tx, rx) = mpsc::channel(32);
        *self.mesh.lock().unwrap() = Some(tx);
        rx
    }

    pub fn port(&self) -> u16 {
        self.endpoint.local_addr().map_or(0, |a| a.port())
    }

    /// Range chaque connexion entrante selon son protocole.
    async fn dispatch(self) {
        while let Some(incoming) = self.endpoint.accept().await {
            // Quelqu'un qui ouvre des milliers de connexions : on refuse le surplus.
            if self.endpoint.open_connections() >= MAX_CONNECTIONS {
                incoming.refuse();
                continue;
            }
            let (host, mesh) = (self.host.clone(), self.mesh.clone());
            tokio::spawn(async move {
                let Ok(conn) = incoming.await else { return };
                let alpn = tls::alpn(&conn);
                let route = match alpn.as_deref() {
                    Some(a) if a == ALPN => host,
                    Some(a) if a == MESH_ALPN => mesh,
                    _ => return conn.close(0u32.into(), b"protocole inconnu"),
                };
                let tx = route.lock().unwrap().clone();
                match tx {
                    Some(tx) => {
                        let _ = tx.send(conn).await;
                    }
                    None => {
                        debug!("{} : connexion refusée (pas d'hôte ici)", conn.remote_address());
                        conn.close(0u32.into(), b"pas d'hote ici");
                    }
                }
            });
        }
    }
}
