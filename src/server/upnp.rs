//! Ouverture automatique du port UDP de l'hôte sur la box (UPnP IGD), pour
//! que des amis hors du réseau local puissent se connecter sans réglage.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use igd_next::{
    PortMappingProtocol, SearchOptions,
    aio::{Gateway, tokio::Tokio},
};
use tracing::{info, warn};

/// Durée du bail demandé à la box ; renouvelé tant que la session tourne.
const LEASE: Duration = Duration::from_secs(3600);
const DESCRIPTION: &str = "RIP Discord";

/// Redirection active sur la box. Appeler [`PortMapping::close`] en partant.
pub struct PortMapping {
    gateway: Gateway<Tokio>,
    local: SocketAddr,
    /// Adresse que les amis doivent utiliser (IP publique de la box + port ouvert).
    pub public: SocketAddr,
    /// Avertissement si l'adresse « publique » ne l'est pas vraiment (CGNAT, double NAT).
    pub warning: Option<String>,
}

impl PortMapping {
    /// Cherche la box et redirige `port` (UDP) vers cette machine.
    pub async fn open(port: u16) -> Result<Self> {
        let mut options = SearchOptions::default();
        options.timeout = Some(Duration::from_secs(4));
        options.single_search_timeout = Some(Duration::from_secs(2));
        let gateway = igd_next::aio::tokio::search_gateway(options)
            .await
            .context("aucune box compatible UPnP trouvée (UPnP désactivé ?)")?;

        let local_ip = route_ip_to(gateway.addr.ip()).context("adresse locale introuvable")?;
        let local = SocketAddr::new(local_ip, port);
        let lease = LEASE.as_secs() as u32;

        // On tente le même port à l'extérieur, sinon un port libre choisi par la box.
        let external_port = match gateway
            .add_port(PortMappingProtocol::UDP, port, local, lease, DESCRIPTION)
            .await
        {
            Ok(()) => port,
            Err(e) => {
                warn!("UPnP : port {port} refusé ({e}), on demande un autre port");
                gateway
                    .add_any_port(PortMappingProtocol::UDP, local, lease, DESCRIPTION)
                    .await
                    .context("la box refuse d'ouvrir un port")?
            }
        };

        let external_ip = gateway.get_external_ip().await.context("IP publique inconnue")?;
        let warning = (!is_public(external_ip)).then(|| {
            format!(
                "La box a ouvert le port, mais son adresse ({external_ip}) n'est pas une adresse Internet : \
                 il y a sans doute une deuxième box devant elle, ou ton opérateur partage ton adresse \
                 (CGNAT). Les amis hors de chez toi risquent de ne pas pouvoir entrer avec ce code."
            )
        });
        let public = SocketAddr::new(external_ip, external_port);
        info!("UPnP : {public} → {local}");
        Ok(Self {
            gateway,
            local,
            public,
            warning,
        })
    }

    /// Renouvelle le bail (à appeler régulièrement, bien avant [`LEASE`]).
    pub async fn renew(&self) -> Result<()> {
        self.gateway
            .add_port(
                PortMappingProtocol::UDP,
                self.public.port(),
                self.local,
                LEASE.as_secs() as u32,
                DESCRIPTION,
            )
            .await?;
        Ok(())
    }

    pub fn renew_interval() -> Duration {
        LEASE / 3
    }

    /// Ferme la redirection sur la box.
    pub async fn close(self) {
        if let Err(e) = self
            .gateway
            .remove_port(PortMappingProtocol::UDP, self.public.port())
            .await
        {
            warn!("UPnP : fermeture du port {} : {e}", self.public.port());
        }
    }
}

/// IP locale utilisée pour joindre `target` (sans envoyer de paquet).
fn route_ip_to(target: IpAddr) -> Result<IpAddr> {
    let bind: SocketAddr = match target {
        IpAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        IpAddr::V6(_) => "[::]:0".parse()?,
    };
    let socket = UdpSocket::bind(bind)?;
    socket.connect((target, 9))?;
    let ip = socket.local_addr()?.ip();
    if ip.is_unspecified() {
        bail!("pas de route vers {target}");
    }
    Ok(ip)
}

/// Adresse joignable depuis Internet (ni privée, ni CGNAT, ni locale).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            let cgnat = a == 100 && (64..128).contains(&b);
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || cgnat)
        }
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xfe00) == 0xfc00),
    }
}

/// IP de cette machine sur le réseau local (pour un code d'invitation « LAN »).
pub fn lan_ip() -> Option<IpAddr> {
    local_ip_address::local_ip().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adresses_publiques() {
        assert!(is_public("8.8.8.8".parse().unwrap()));
        assert!(!is_public("192.168.1.1".parse().unwrap()));
        assert!(!is_public("10.0.0.1".parse().unwrap()));
        assert!(!is_public("100.70.1.1".parse().unwrap()));
        assert!(is_public("2a01:e0a::1".parse().unwrap()));
    }
}
