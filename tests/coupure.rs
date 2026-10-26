//! Réseau qui flanche : Wi-Fi qui décroche, PC qui sort de veille. Un membre
//! passe par un petit relais UDP qu'on peut couper ; une courte coupure ne doit
//! rien déclencher, une longue doit le ramener chez le même hôte, avec son
//! numéro, sans qu'un second serveur apparaisse.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rip_discord::{
    client::{
        Client, Event, Options,
        failover::{self, Outcome, Plan},
        station::Station,
    },
    core::{
        card::ServerCard,
        identity::Identity,
        protocol::{Contact, Reach, RelayOffer},
        store::HistoryLog,
    },
    server::Host,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::UnboundedReceiver,
    time::{sleep, timeout},
};

/// Relais UDP entre un membre et l'hôte ; `cut` jette tout ce qui passe.
async fn flaky_link(host: SocketAddr) -> (SocketAddr, Arc<AtomicBool>) {
    let outer = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    inner.connect(host).await.unwrap();
    let addr = outer.local_addr().unwrap();
    let cut = Arc::new(AtomicBool::new(false));
    let member = Arc::new(tokio::sync::Mutex::new(None::<SocketAddr>));
    {
        let (outer, inner, cut, member) = (outer.clone(), inner.clone(), cut.clone(), member.clone());
        tokio::spawn(async move {
            let mut buf = vec![0; 65536];
            while let Ok((n, from)) = outer.recv_from(&mut buf).await {
                *member.lock().await = Some(from);
                if !cut.load(Ordering::Relaxed) {
                    let _ = inner.send(&buf[..n]).await;
                }
            }
        });
    }
    {
        let cut = cut.clone();
        tokio::spawn(async move {
            let mut buf = vec![0; 65536];
            while let Ok(n) = inner.recv(&mut buf).await {
                let to = *member.lock().await;
                if let Some(to) = to
                    && !cut.load(Ordering::Relaxed)
                {
                    let _ = outer.send_to(&buf[..n], to).await;
                }
            }
        });
    }
    (addr, cut)
}

async fn wait_for(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    loop {
        let ev = timeout(Duration::from_secs(20), rx.recv())
            .await
            .expect("délai dépassé")
            .expect("canal fermé");
        if pred(&ev) {
            return ev;
        }
    }
}

struct Member {
    identity: Identity,
    station: Station,
    client: Client,
    events: UnboundedReceiver<Event>,
}

async fn member(addr: SocketAddr, card: &ServerCard, name: &str) -> Member {
    let identity = Identity::generate();
    let station = Station::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let opts = Options {
        key: Some(card.access_key()),
        ..Default::default()
    };
    let (client, events) = Client::connect_with(station.endpoint.clone(), addr, &identity, name, &opts)
        .await
        .unwrap();
    client
        .offer_relay(RelayOffer {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], station.port()))],
            cert: station.cert.fingerprint,
            reach: Reach::Open,
        })
        .unwrap();
    Member {
        identity,
        station,
        client,
        events,
    }
}

async fn succession(m: &mut Member, heirs: usize) -> Vec<Contact> {
    match wait_for(
        &mut m.events,
        |e| matches!(e, Event::Succession(c) if c.iter().filter(|c| c.heir).count() == heirs),
    )
    .await
    {
        Event::Succession(c) => c,
        _ => unreachable!(),
    }
}

/// Hôte, Carol branchée directement, Bob derrière le relais coupable.
async fn setup(name: &str) -> (ServerCard, SocketAddr, Member, Member, Arc<AtomicBool>, Vec<Contact>) {
    let card = ServerCard::create(&Identity::generate(), name);
    let host = Host::bind(
        "127.0.0.1:0".parse().unwrap(),
        card.clone(),
        HistoryLog::in_memory(card.id, card.history()),
    )
    .unwrap();
    let addr = host.local_addr().unwrap();
    tokio::spawn(host.run());
    let (via, cut) = flaky_link(addr).await;
    let mut bob = member(via, &card, "bob").await;
    let mut carol = member(addr, &card, "carol").await;
    let order = succession(&mut bob, 2).await;
    succession(&mut carol, 2).await;
    (card, via, bob, carol, cut, order)
}

#[tokio::test]
async fn courte_coupure_rien_ne_bouge() {
    let (card, _, mut bob, mut carol, cut, _) = setup("coupure courte").await;

    // Deux secondes sans réseau (moins que le délai de détection).
    cut.store(true, Ordering::Relaxed);
    sleep(Duration::from_secs(2)).await;
    cut.store(false, Ordering::Relaxed);

    // Toujours la même connexion : le texte passe dans les deux sens, et
    // personne n'a vu Bob partir.
    let general = card.settings().text_channels[0].id;
    let sent = bob.client.send_text(general, "je suis là").unwrap();
    let ev = wait_for(&mut carol.events, |e| {
        matches!(e, Event::Text { .. } | Event::PeerLeft { .. })
    })
    .await;
    assert!(matches!(ev, Event::Text { msg, .. } if msg == sent));
    let sent = carol.client.send_text(general, "moi aussi").unwrap();
    let ev = wait_for(&mut bob.events, |e| {
        matches!(e, Event::Text { .. } | Event::Disconnected { .. })
    })
    .await;
    assert!(matches!(ev, Event::Text { msg, .. } if msg == sent));
}

#[tokio::test]
async fn longue_coupure_on_revient_chez_le_meme_hote() {
    let (card, via, mut bob, mut carol, cut, order) = setup("coupure longue").await;
    let bob_id = bob.client.peer_id;

    // Veille, ou Wi-Fi perdu plus longtemps que le délai de détection.
    cut.store(true, Ordering::Relaxed);
    wait_for(&mut bob.events, |e| matches!(e, Event::Disconnected { .. })).await;
    cut.store(false, Ordering::Relaxed);

    // L'appli suit l'ordre de succession, mais l'hôte est toujours là :
    // Bob doit le retrouver, pas monter un second serveur.
    let plan = Plan {
        key: card.access_key(),
        old_host: via,
        old_cert: bob.client.host_cert(),
        contacts: order,
        graceful: false,
        me: bob_id,
    };
    assert!(plan.old_cert.is_some(), "certificat de l'hôte connu");
    let outcome = failover::follow(&plan, &bob.station.endpoint, &bob.identity, "bob", |_| {}).await;
    let Outcome::Joined(client, _events) = outcome else {
        panic!("Bob aurait dû retrouver l'hôte");
    };
    assert_eq!(client.peer_id, bob_id, "Bob garde son numéro");

    // Carol le voit toujours (ou de nouveau) et lui écrit.
    let general = card.settings().text_channels[0].id;
    let sent = client.send_text(general, "de retour").unwrap();
    wait_for(
        &mut carol.events,
        |e| matches!(e, Event::Text { msg, .. } if *msg == sent),
    )
    .await;
}
