//! Traversée de box : savoir sous quelle adresse publique la box présente le
//! port QUIC de ce PC, et garder cette « ouverture » vivante.
//!
//! On interroge des serveurs STUN publics (Google, Cloudflare : rien à
//! installer, ils ne voient qu'une requête vide) **depuis le socket même de
//! QUIC**, grâce à [`StunSocket`] qui enveloppe ce socket et intercepte les
//! réponses STUN avant QUIC. L'adresse obtenue est celle que les autres PC
//! doivent viser pour la perforation.

use std::{
    fmt,
    io::{self, IoSliceMut},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket as StdUdpSocket},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use quinn::{
    AsyncUdpSocket, Endpoint, EndpointConfig, Runtime, TokioRuntime, UdpPoller,
    udp::{RecvMeta, Transmit},
};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::watch;
use tracing::debug;

/// Serveurs STUN publics par défaut ; `RIP_DISCORD_STUN` (liste séparée par des
/// virgules) les remplace.
const DEFAULT_STUN: &[&str] = &["stun.l.google.com:19302", "stun.cloudflare.com:3478"];
const MAGIC_COOKIE: u32 = 0x2112_A442;

pub fn stun_servers() -> Vec<String> {
    match std::env::var("RIP_DISCORD_STUN") {
        Ok(list) => list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        Err(_) => DEFAULT_STUN.iter().map(|s| s.to_string()).collect(),
    }
}

/// Socket QUIC qui laisse aussi passer des requêtes STUN.
pub struct StunSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    /// Dernière adresse publique vue par chaque serveur STUN.
    seen: watch::Sender<Vec<(SocketAddr, SocketAddr)>>,
    pending: Mutex<Vec<[u8; 12]>>,
}

impl fmt::Debug for StunSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StunSocket").field("inner", &self.inner).finish()
    }
}

/// Crée un endpoint QUIC (serveur si `server` est fourni) sur `addr`, capable
/// de découvrir son adresse publique.
pub fn endpoint(addr: SocketAddr, server: Option<quinn::ServerConfig>) -> Result<(Endpoint, Probe)> {
    let socket = StdUdpSocket::bind(addr).with_context(|| format!("impossible d'écouter sur {addr}"))?;
    let runtime: Arc<dyn Runtime> = Arc::new(TokioRuntime);
    let inner = runtime.wrap_udp_socket(socket)?;
    let (seen, rx) = watch::channel(Vec::new());
    let stun = Arc::new(StunSocket {
        inner,
        seen,
        pending: Mutex::new(Vec::new()),
    });
    let endpoint = Endpoint::new_with_abstract_socket(EndpointConfig::default(), server, stun.clone(), runtime)?;
    Ok((endpoint, Probe { socket: stun, seen: rx }))
}

impl AsyncUdpSocket for StunSocket {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let polled = self.inner.poll_recv(cx, bufs, meta);
        if let Poll::Ready(Ok(n)) = &polled {
            for (m, buf) in meta.iter_mut().zip(bufs.iter()).take(*n) {
                let data = &buf[..m.len];
                if let Some((txid, mapped)) = parse_response(data)
                    && self.take_pending(txid)
                {
                    debug!("STUN {} : adresse publique {mapped}", m.addr);
                    self.seen.send_modify(|v| {
                        v.retain(|(server, _)| *server != m.addr);
                        v.push((m.addr, mapped));
                    });
                    // Vide pour QUIC, qui l'ignore.
                    m.len = 0;
                }
            }
        }
        polled
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}

impl StunSocket {
    fn take_pending(&self, txid: [u8; 12]) -> bool {
        let mut pending = self.pending.lock().unwrap();
        match pending.iter().position(|t| *t == txid) {
            Some(i) => {
                pending.swap_remove(i);
                true
            }
            None => false,
        }
    }

    fn send_request(&self, to: SocketAddr) -> io::Result<()> {
        let mut txid = [0u8; 12];
        SystemRandom::new()
            .fill(&mut txid)
            .map_err(|_| io::Error::other("aléa indisponible"))?;
        {
            let mut pending = self.pending.lock().unwrap();
            pending.push(txid);
            let excess = pending.len().saturating_sub(32);
            pending.drain(..excess);
        }
        let request = binding_request(txid);
        let transmit = Transmit {
            destination: to,
            ecn: None,
            contents: &request,
            segment_size: None,
            src_ip: None,
        };
        // Le socket peut être momentanément plein : quelques essais suffisent.
        for _ in 0..10 {
            match self.inner.try_send(&transmit) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)),
                other => return other,
            }
        }
        Err(io::ErrorKind::WouldBlock.into())
    }
}

/// Ce que la box fait du port QUIC de ce PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mapping {
    /// Même adresse publique quel que soit le destinataire : la perforation marche.
    Stable(SocketAddr),
    /// Adresse différente selon le destinataire (NAT « symétrique ») : la
    /// perforation échouera le plus souvent, il faudra un relais.
    Symmetric(SocketAddr),
}

impl Mapping {
    pub fn addr(&self) -> SocketAddr {
        match self {
            Mapping::Stable(a) | Mapping::Symmetric(a) => *a,
        }
    }
}

/// Poignée pour interroger STUN depuis le socket d'un endpoint.
#[derive(Clone)]
pub struct Probe {
    socket: Arc<StunSocket>,
    seen: watch::Receiver<Vec<(SocketAddr, SocketAddr)>>,
}

impl Probe {
    /// Interroge les serveurs STUN et renvoie l'adresse publique de ce port.
    pub async fn discover(&mut self) -> Result<Mapping> {
        let mut servers = Vec::new();
        for spec in stun_servers() {
            match tokio::net::lookup_host(&spec).await {
                Ok(mut addrs) => {
                    if let Some(a) = addrs.find(|a| a.is_ipv4()) {
                        servers.push(a);
                    }
                }
                Err(e) => debug!("STUN {spec} : {e}"),
            }
        }
        if servers.is_empty() {
            bail!("aucun serveur STUN joignable");
        }
        self.socket.seen.send_replace(Vec::new());
        for attempt in 0..3 {
            for s in &servers {
                let _ = self.socket.send_request(*s);
            }
            let wait = Duration::from_millis(400 * (attempt + 1));
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                let got = self.seen.borrow().clone();
                if got.len() >= servers.len().min(2) {
                    return Ok(classify(&got));
                }
                if tokio::time::timeout_at(deadline, self.seen.changed()).await.is_err() {
                    break;
                }
            }
            let got = self.seen.borrow().clone();
            if !got.is_empty() && attempt == 2 {
                return Ok(classify(&got));
            }
        }
        let got = self.seen.borrow().clone();
        if got.is_empty() {
            bail!("pas de réponse des serveurs STUN (réseau sans Internet ?)");
        }
        Ok(classify(&got))
    }
}

fn classify(seen: &[(SocketAddr, SocketAddr)]) -> Mapping {
    let first = seen[0].1;
    if seen.iter().all(|(_, m)| *m == first) {
        Mapping::Stable(first)
    } else {
        Mapping::Symmetric(first)
    }
}

fn binding_request(txid: [u8; 12]) -> [u8; 20] {
    let mut r = [0u8; 20];
    r[..2].copy_from_slice(&0x0001u16.to_be_bytes());
    r[4..8].copy_from_slice(&MAGIC_COOKIE.to_be_bytes());
    r[8..].copy_from_slice(&txid);
    r
}

/// Réponse « Binding Success » : identifiant de transaction et adresse vue.
fn parse_response(data: &[u8]) -> Option<([u8; 12], SocketAddr)> {
    if data.len() < 20 || data[..2] != [0x01, 0x01] || data[4..8] != MAGIC_COOKIE.to_be_bytes() {
        return None;
    }
    let len = u16::from_be_bytes([data[2], data[3]]) as usize;
    let txid: [u8; 12] = data[8..20].try_into().ok()?;
    let mut attrs = data.get(20..20 + len)?;
    let mut mapped = None;
    while attrs.len() >= 4 {
        let kind = u16::from_be_bytes([attrs[0], attrs[1]]);
        let alen = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let value = attrs.get(4..4 + alen)?;
        match kind {
            0x0020 => return Some((txid, decode_addr(value, true, &txid)?)),
            0x0001 => mapped = decode_addr(value, false, &txid),
            _ => {}
        }
        attrs = attrs.get(4 + alen.div_ceil(4) * 4..).unwrap_or(&[]);
    }
    Some((txid, mapped?))
}

fn decode_addr(v: &[u8], xor: bool, txid: &[u8; 12]) -> Option<SocketAddr> {
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let mut port = u16::from_be_bytes([*v.get(2)?, *v.get(3)?]);
    if xor {
        port ^= (MAGIC_COOKIE >> 16) as u16;
    }
    let ip: IpAddr = match v.get(1)? {
        1 => {
            let mut b: [u8; 4] = v.get(4..8)?.try_into().ok()?;
            if xor {
                for (x, c) in b.iter_mut().zip(cookie) {
                    *x ^= c;
                }
            }
            Ipv4Addr::from(b).into()
        }
        2 => {
            let mut b: [u8; 16] = v.get(4..20)?.try_into().ok()?;
            if xor {
                let key: Vec<u8> = cookie.iter().chain(txid).copied().collect();
                for (x, k) in b.iter_mut().zip(key) {
                    *x ^= k;
                }
            }
            Ipv6Addr::from(b).into()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// Réponse STUN minimale (XOR-MAPPED-ADDRESS, IPv4) : sert aux tests et au
/// réseau de développement (`rip-discord dev-network`).
pub fn stun_reply(request: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
    if request.len() < 20 || request[..2] != [0x00, 0x01] || request[4..8] != MAGIC_COOKIE.to_be_bytes() {
        return None;
    }
    let SocketAddr::V4(from) = from else { return None };
    let mut r = Vec::with_capacity(32);
    r.extend_from_slice(&[0x01, 0x01, 0x00, 12]);
    r.extend_from_slice(&request[4..20]);
    r.extend_from_slice(&[0x00, 0x20, 0x00, 0x08, 0x00, 0x01]);
    r.extend_from_slice(&(from.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
    for (o, c) in from.ip().octets().iter().zip(MAGIC_COOKIE.to_be_bytes()) {
        r.push(o ^ c);
    }
    Some(r)
}

/// Petit serveur STUN (tests, réseau de développement).
pub async fn run_stun_server(socket: tokio::net::UdpSocket) {
    let mut buf = [0u8; 1500];
    while let Ok((n, from)) = socket.recv_from(&mut buf).await {
        if let Some(reply) = stun_reply(&buf[..n], from) {
            let _ = socket.send_to(&reply, from).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requete_et_reponse() {
        let req = binding_request([7; 12]);
        let from: SocketAddr = "203.0.113.7:51234".parse().unwrap();
        let reply = stun_reply(&req, from).unwrap();
        assert_eq!(parse_response(&reply), Some(([7; 12], from)));
        assert!(parse_response(&req).is_none());
        assert!(parse_response(b"un paquet QUIC quelconque.....").is_none());
    }

    #[tokio::test]
    async fn decouvre_son_adresse_depuis_le_socket_quic() {
        let stun = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let stun_addr = stun.local_addr().unwrap();
        tokio::spawn(run_stun_server(stun));
        // SAFETY : variable lue uniquement par ce processus de test.
        unsafe { std::env::set_var("RIP_DISCORD_STUN", stun_addr.to_string()) };

        let (endpoint, mut probe) = endpoint("127.0.0.1:0".parse().unwrap(), None).unwrap();
        let mapping = probe.discover().await.unwrap();
        assert_eq!(mapping, Mapping::Stable(endpoint.local_addr().unwrap()));
    }
}
