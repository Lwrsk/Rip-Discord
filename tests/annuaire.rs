//! Annuaire décentralisé : un réseau DHT et un serveur STUN locaux remplacent
//! ceux d'Internet.

use std::{net::UdpSocket, sync::OnceLock, time::Duration};

use rip_discord::{
    client,
    core::{card::ServerCard, directory::Directory, identity::Identity, invite::Invite, nat, store::HistoryLog},
    server::{
        Host,
        announce::{Hosting, announce},
    },
};
use tokio::time::timeout;

/// Réseau partagé par tous les tests de ce fichier.
fn network() {
    static NET: OnceLock<()> = OnceLock::new();
    NET.get_or_init(|| {
        use tracing_subscriber::{filter::Targets, prelude::*};
        let filter: Targets = std::env::var("RUST_LOG")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| Targets::new().with_default(tracing::level_filters::LevelFilter::ERROR));
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_test_writer())
            .try_init();
        let testnet = mainline::Testnet::builder(10).build().unwrap();
        let stun = UdpSocket::bind("127.0.0.1:0").unwrap();
        // SAFETY : fixées une seule fois, avant tout usage du réseau.
        unsafe {
            std::env::set_var("RIP_DISCORD_DHT_BOOTSTRAP", testnet.bootstrap.join(","));
            std::env::set_var("RIP_DISCORD_STUN", stun.local_addr().unwrap().to_string());
        }
        testnet.leak();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            while let Ok((n, from)) = stun.recv_from(&mut buf) {
                if let Some(reply) = nat::stun_reply(&buf[..n], from) {
                    let _ = stun.send_to(&reply, from);
                }
            }
        });
    });
}

fn bind(card: &ServerCard) -> Host {
    Host::bind(
        "0.0.0.0:0".parse().unwrap(),
        card.clone(),
        HistoryLog::in_memory(card.id, card.history()),
    )
    .unwrap()
}

/// Le code « serveur » mène à l'hôte du moment ; un second membre qui voudrait
/// héberger en même temps est renvoyé vers lui ; quand l'hôte part, le
/// serveur n'est plus annoncé.
#[tokio::test(flavor = "multi_thread")]
async fn rejoindre_via_l_annuaire() {
    network();
    let dir = Directory::start().await.unwrap();
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "Les potes");
    let code = Invite::key(card.access_key());

    // Personne n'héberge encore.
    let err = client::join(&code, &owner, "alice").await.err().unwrap();
    assert!(format!("{err:#}").contains("Personne n'héberge"), "{err:#}");

    let host = bind(&card);
    let Hosting::Announced(announcement) = announce(&dir, &host, owner.id()).await.unwrap() else {
        panic!("annonce refusée");
    };
    let port = host.local_addr().unwrap().port();
    assert_eq!(announcement.mapping.addr().port(), port);
    let endpoint = host.endpoint();
    tokio::spawn(host.run());

    let (bob, _bob_rx) = timeout(
        Duration::from_secs(30),
        client::join(&code, &Identity::generate(), "bob"),
    )
    .await
    .expect("délai dépassé")
    .unwrap();
    assert_eq!(bob.card.id, card.id);

    // Un autre membre tente d'héberger le même serveur : il est renvoyé vers l'hôte.
    let other = bind(&card);
    match announce(&dir, &other, Identity::generate().id()).await.unwrap() {
        Hosting::AlreadyHosted(h) => {
            assert_eq!(h.user, owner.id());
            assert_eq!(h.addr.public.port(), port);
        }
        Hosting::Announced(_) => panic!("deux hôtes pour un même serveur"),
    }

    // Un code qui vise un autre serveur ne trouve personne.
    let wrong = Invite::key([0; 16]);
    assert!(client::join(&wrong, &owner, "alice").await.is_err());

    // L'hôte s'en va : le serveur n'est plus annoncé.
    bob.quit().await;
    announcement.stop(false).await;
    endpoint.close(0u32.into(), b"bye");
    let err = client::join(&code, &owner, "alice").await.err().unwrap();
    assert!(format!("{err:#}").contains("Personne n'héberge"), "{err:#}");
}
