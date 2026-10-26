//! Relève d'hôte : quand l'hôte part ou disparaît, un membre reprend la
//! session sans que personne n'ait à cliquer.
//!
//! 1. Chaque membre propose à l'hôte de quoi le joindre ([`RelayOffer`]) :
//!    les adresses de son port QUIC (déjà ouvert dans sa box, puisqu'il parle
//!    à l'hôte) et l'empreinte du certificat qu'il présentera.
//! 2. L'hôte en tire un ordre de succession ([`succession`]), du membre le plus
//!    facile à joindre au plus difficile, et le diffuse à tous à chaque
//!    changement (`Control::Succession`).
//! 3. L'hôte part (`Control::HostLeaving`) ou ne répond plus (keep-alive QUIC) :
//!    chacun suit la même liste. Le premier successeur se met à héberger sur
//!    son port, perce sa box vers les autres ; les autres s'y connectent (en
//!    vérifiant son certificat) et retrouvent leur numéro (`Hello::resume`).
//!    S'il ne répond pas, on passe au suivant ; en dernier recours, l'annuaire.

use std::net::SocketAddr;

use crate::core::{
    identity::UserId,
    protocol::{Contact, PeerId, RelayOffer},
};

/// Un membre tel que l'hôte le connaît.
pub struct Candidate<'a> {
    pub peer: PeerId,
    pub user: UserId,
    pub offer: &'a RelayOffer,
    /// Adresse sous laquelle l'hôte voit le membre (ajoutée aux siennes).
    pub seen: SocketAddr,
    /// Peut prendre la relève (faux pour l'hôte lui-même).
    pub heir: bool,
}

/// Contacts dans l'ordre de succession : `preferred` d'abord (élu par vote),
/// puis les mieux joignables, puis les plus anciens ; ceux qui ne peuvent
/// pas prendre la relève en dernier.
pub fn succession<'a>(candidates: impl IntoIterator<Item = Candidate<'a>>, preferred: Option<PeerId>) -> Vec<Contact> {
    let mut v: Vec<_> = candidates.into_iter().collect();
    v.sort_by_key(|c| (!c.heir, Some(c.peer) != preferred, c.offer.reach, c.peer));
    v.into_iter()
        .map(|c| {
            let mut addrs = c.offer.addrs.clone();
            if !c.seen.ip().is_loopback() && !c.seen.ip().is_unspecified() && !addrs.contains(&c.seen) {
                addrs.push(c.seen);
            }
            Contact {
                peer: c.peer,
                user: c.user,
                addrs,
                cert: c.offer.cert,
                heir: c.heir,
            }
        })
        .collect()
}

/// Hôte d'un salon vocal : toujours un participant du salon (le premier arrivé
/// encore présent), puisque c'est lui qui mixera et relaiera l'audio. Quand il
/// part, le suivant prend le relais.
pub fn elect_voice_host(members: &[PeerId]) -> Option<PeerId> {
    members.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::protocol::Reach;

    fn offer(reach: Reach, port: u16) -> RelayOffer {
        RelayOffer {
            addrs: vec![SocketAddr::from(([192, 168, 1, 10], port))],
            cert: [port as u8; 32],
            reach,
        }
    }

    fn cand(peer: PeerId, offer: &RelayOffer, heir: bool) -> Candidate<'_> {
        Candidate {
            peer,
            user: UserId([peer as u8; 32]),
            offer,
            seen: "198.51.100.4:4000".parse().unwrap(),
            heir,
        }
    }

    #[test]
    fn mieux_joignable_puis_plus_ancien() {
        let (a, b, c) = (
            offer(Reach::Symmetric, 1),
            offer(Reach::Stable, 2),
            offer(Reach::Stable, 3),
        );
        let heirs = succession([cand(2, &a, true), cand(7, &b, true), cand(4, &c, true)], None);
        let order: Vec<_> = heirs.iter().map(|h| h.peer).collect();
        assert_eq!(order, [4, 7, 2]);
        // L'adresse vue par l'hôte s'ajoute à celles annoncées.
        assert_eq!(heirs[0].addrs.len(), 2);
        assert_eq!(heirs[0].cert, [3; 32]);
    }

    #[test]
    fn elu_par_vote_d_abord_hote_en_dernier() {
        let (a, b) = (offer(Reach::Open, 1), offer(Reach::Symmetric, 2));
        let heirs = succession([cand(1, &a, false), cand(2, &a, true), cand(3, &b, true)], Some(3));
        let order: Vec<_> = heirs.iter().map(|h| (h.peer, h.heir)).collect();
        assert_eq!(order, [(3, true), (2, true), (1, false)]);
    }

    #[test]
    fn adresse_locale_vue_par_l_hote_ignoree() {
        let o = offer(Reach::Open, 1);
        let mut c = cand(1, &o, true);
        c.seen = "127.0.0.1:5000".parse().unwrap();
        let heirs = succession([c], None);
        assert_eq!(heirs[0].addrs, o.addrs);
    }

    #[test]
    fn hote_vocal_parmi_les_participants() {
        assert_eq!(elect_voice_host(&[5, 2, 9]), Some(5));
        assert_eq!(elect_voice_host(&[]), None);
    }
}
