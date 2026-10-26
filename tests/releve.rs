//! Relève d'hôte de bout en bout, en local : l'hôte part, un membre reprend
//! le serveur sur son propre port et les autres le suivent sans repasser par
//! l'annuaire, en gardant leur numéro.

use std::{net::SocketAddr, time::Duration};

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
use tokio::{sync::mpsc::UnboundedReceiver, time::timeout};

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

/// Un membre : sa station (son port pour toute la session) et sa connexion.
struct Member {
    identity: Identity,
    name: &'static str,
    station: Station,
    client: Client,
    events: UnboundedReceiver<Event>,
}

async fn member(addr: SocketAddr, card: &ServerCard, name: &'static str, identity: Identity) -> Member {
    let station = Station::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let opts = Options {
        key: Some(card.access_key()),
        ..Default::default()
    };
    let (client, events) = Client::connect_with(station.endpoint.clone(), addr, &identity, name, &opts)
        .await
        .unwrap();
    offer(&client, &station);
    Member {
        identity,
        name,
        station,
        client,
        events,
    }
}

/// Peut prendre la relève : voici comment me joindre.
fn offer(client: &Client, station: &Station) {
    client
        .offer_relay(RelayOffer {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], station.port()))],
            cert: station.cert.fingerprint,
            reach: Reach::Open,
        })
        .unwrap();
}

/// Attend un ordre de succession où tous ces membres peuvent reprendre.
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

/// Ce que fait l'appli quand l'hôte disparaît : suivre l'ordre, et héberger
/// si c'est son tour.
async fn follow(m: &Member, plan: Plan, card: &ServerCard) -> (Client, UnboundedReceiver<Event>, bool) {
    match failover::follow(&plan, &m.station.endpoint, &m.identity, m.name, |_| {}).await {
        Outcome::Joined(client, events) => (*client, events, false),
        Outcome::TakeOver => {
            let host = Host::on_station(
                &m.station,
                card.clone(),
                HistoryLog::in_memory(card.id, card.history()),
                plan.contacts.iter().map(|c| c.peer).max().unwrap_or(0) + 1,
            )
            .unwrap()
            .with_operator(m.identity.id());
            tokio::spawn(host.run());
            let local = SocketAddr::from(([127, 0, 0, 1], m.station.port()));
            let opts = Options {
                key: Some(card.access_key()),
                pin: Some(m.station.cert.fingerprint.to_vec()),
                resume: Some(plan.me),
            };
            let (client, events) = Client::connect_with(m.station.endpoint.clone(), local, &m.identity, m.name, &opts)
                .await
                .unwrap();
            (client, events, true)
        }
        Outcome::Lost => panic!("{} : personne n'a repris le serveur", m.name),
    }
}

fn plan(m: &Member, card: &ServerCard, old_host: SocketAddr, contacts: Vec<Contact>, graceful: bool) -> Plan {
    Plan {
        key: card.access_key(),
        old_host,
        old_cert: m.client.host_cert(),
        contacts,
        graceful,
        me: m.client.peer_id,
    }
}

fn start_host(card: &ServerCard) -> (SocketAddr, rip_discord::server::HostHandle) {
    let host = Host::bind(
        "127.0.0.1:0".parse().unwrap(),
        card.clone(),
        HistoryLog::in_memory(card.id, card.history()),
    )
    .unwrap();
    let addr = host.local_addr().unwrap();
    let handle = host.handle();
    tokio::spawn(host.run());
    (addr, handle)
}

#[tokio::test]
async fn hote_parti_un_membre_reprend_sans_deconnecter_personne() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "relève");
    let (addr, handle) = start_host(&card);

    let mut bob = member(addr, &card, "bob", Identity::generate()).await;
    let mut carol = member(addr, &card, "carol", Identity::generate()).await;
    let order = succession(&mut carol, 2).await;
    succession(&mut bob, 2).await;
    let (bob_id, carol_id) = (bob.client.peer_id, carol.client.peer_id);

    // L'hôte s'arrête net (sans prévenir les membres de la relève).
    handle.leave(false).await;

    let bob_plan = plan(&bob, &card, addr, order.clone(), false);
    let carol_plan = plan(&carol, &card, addr, order.clone(), false);
    let ((b, mut b_events, b_hosts), (c, _c_events, c_hosts)) =
        tokio::join!(follow(&bob, bob_plan, &card), follow(&carol, carol_plan, &card));

    // Exactement un des deux héberge : le premier de l'ordre de succession.
    assert!(b_hosts ^ c_hosts, "un seul nouvel hôte");
    let heir = order.iter().find(|c| c.heir).unwrap().peer;
    assert_eq!(b_hosts, heir == bob_id);
    // Chacun garde son numéro : le salon, les volumes et les statuts suivent.
    assert_eq!((b.peer_id, c.peer_id), (bob_id, carol_id));

    // Et le texte repasse.
    let general = card.settings().text_channels[0].id;
    let sent = c.send_text(general, "toujours là ?").unwrap();
    match wait_for(&mut b_events, |e| matches!(e, Event::Text { .. })).await {
        Event::Text { from, msg } => {
            assert_eq!(from, carol_id);
            assert_eq!(msg, sent);
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn vote_a_la_majorite_pour_changer_d_hote() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "vote");
    let (addr, _handle) = start_host(&card);

    let mut bob = member(addr, &card, "bob", Identity::generate()).await;
    let mut carol = member(addr, &card, "carol", Identity::generate()).await;
    succession(&mut bob, 2).await;
    let mut order = succession(&mut carol, 2).await;
    // Le candidat est celui qui n'aurait pas repris de lui-même.
    if order.iter().find(|c| c.heir).unwrap().peer == bob.client.peer_id {
        std::mem::swap(&mut bob, &mut carol);
    }
    let bob_id = bob.client.peer_id;

    // Bob trouve que ça lag : il demande à héberger, Carol vote oui.
    bob.client.request_host().unwrap();
    let id = match wait_for(&mut carol.events, |e| matches!(e, Event::Vote { .. })).await {
        Event::Vote { id, candidate, .. } => {
            assert_eq!(candidate, bob_id);
            id
        }
        _ => unreachable!(),
    };
    carol.client.ballot(id, true).unwrap();
    match wait_for(&mut bob.events, |e| matches!(e, Event::VoteResult { .. })).await {
        Event::VoteResult { accepted, yes, no, .. } => assert_eq!((accepted, yes, no), (true, 2, 0)),
        _ => unreachable!(),
    }

    // L'hôte passe la main : Bob est désormais premier dans l'ordre.
    while let Event::Succession(c) = wait_for(&mut carol.events, |e| {
        matches!(e, Event::Succession(_) | Event::HostLeaving)
    })
    .await
    {
        order = c;
    }
    assert_eq!(order.iter().find(|c| c.heir).unwrap().peer, bob_id);

    let bob_plan = plan(&bob, &card, addr, order.clone(), true);
    let carol_plan = plan(&carol, &card, addr, order.clone(), true);
    let ((_b, _, b_hosts), (c, _, c_hosts)) =
        tokio::join!(follow(&bob, bob_plan, &card), follow(&carol, carol_plan, &card));
    assert!(b_hosts && !c_hosts);
    assert_eq!(c.peer_id, carol.client.peer_id);
}

/// Le PC d'un membre s'éteint d'un coup : plus rien ne part de son port.
fn crash(m: Member) {
    m.station.endpoint.close(0u32.into(), b"panne");
}

/// Un hôte qu'on peut faire tomber sans qu'il prévienne personne.
fn start_crashable_host(card: &ServerCard) -> (SocketAddr, quinn::Endpoint) {
    let host = Host::bind(
        "127.0.0.1:0".parse().unwrap(),
        card.clone(),
        HistoryLog::in_memory(card.id, card.history()),
    )
    .unwrap();
    let addr = host.local_addr().unwrap();
    let endpoint = host.endpoint();
    tokio::spawn(host.run());
    (addr, endpoint)
}

/// Fait suivre la relève à tous les survivants en même temps ; renvoie les
/// membres reconnectés et l'adresse du nouvel hôte.
async fn everyone_follows(
    members: Vec<Member>,
    card: &ServerCard,
    old_host: SocketAddr,
    order: &[Contact],
) -> (Vec<Member>, SocketAddr) {
    let mut runs = tokio::task::JoinSet::new();
    for m in members {
        let (card, order) = (card.clone(), order.to_vec());
        runs.spawn(async move {
            let p = plan(&m, &card, old_host, order, false);
            let before = m.client.peer_id;
            let (client, events, hosts) = follow(&m, p, &card).await;
            assert_eq!(client.peer_id, before, "{} a perdu son numéro", m.name);
            // Comme l'appli : on se repropose au nouvel hôte.
            offer(&client, &m.station);
            (Member { client, events, ..m }, hosts)
        });
    }
    let done = runs.join_all().await;
    let hosts: Vec<_> = done.iter().filter(|(_, h)| *h).map(|(m, _)| m.name).collect();
    assert_eq!(hosts.len(), 1, "un seul nouvel hôte, pas {hosts:?}");
    let new_host = done.iter().find(|(_, h)| *h).map(|(m, _)| m.station.port()).unwrap();
    let members = done.into_iter().map(|(m, _)| m).collect();
    (members, SocketAddr::from(([127, 0, 0, 1], new_host)))
}

/// Le premier de l'ordre de succession parmi `members`.
fn first_heir<'a>(order: &[Contact], members: &'a [Member]) -> &'a Member {
    let peer = order.iter().find(|c| c.heir).unwrap().peer;
    members.iter().find(|m| m.client.peer_id == peer).unwrap()
}

/// Vérifie que le texte passe entre tous les membres.
async fn text_flows(members: &mut [Member], card: &ServerCard) {
    let general = card.settings().text_channels[0].id;
    let (first, others) = members.split_first_mut().unwrap();
    let sent = first
        .client
        .send_text(general, format!("{} : toujours là ?", first.name))
        .unwrap();
    for m in others {
        match wait_for(&mut m.events, |e| matches!(e, Event::Text { msg, .. } if *msg == sent)).await {
            Event::Text { from, .. } => assert_eq!(from, first.client.peer_id),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn tempete_les_hotes_tombent_l_un_apres_l_autre() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "tempête");
    let (addr, host) = start_crashable_host(&card);

    let mut members = Vec::new();
    for name in ["ana", "bob", "cid", "dan"] {
        members.push(member(addr, &card, name, Identity::generate()).await);
    }
    let mut order = Vec::new();
    for m in &mut members {
        order = succession(m, 4).await;
    }

    // 1. L'hôte et son premier successeur tombent en même temps : on passe
    //    au deuxième de la liste.
    let heir = first_heir(&order, &members).client.peer_id;
    let i = members.iter().position(|m| m.client.peer_id == heir).unwrap();
    host.close(0u32.into(), b"panne");
    crash(members.remove(i));
    let second = order.iter().filter(|c| c.heir).nth(1).unwrap().peer;
    let (mut members, addr) = everyone_follows(members, &card, addr, &order).await;
    let new_host = members.iter().find(|m| m.station.port() == addr.port()).unwrap();
    assert_eq!(new_host.client.peer_id, second);
    // Tous se reproposent au nouvel hôte, qui en tire un nouvel ordre.
    for m in &mut members {
        order = succession(m, 2).await;
    }
    text_flows(&mut members, &card).await;

    // 2. Le nouvel hôte tombe à son tour : les deux derniers continuent.
    let i = members.iter().position(|m| m.station.port() == addr.port()).unwrap();
    crash(members.remove(i));
    let (mut members, _) = everyone_follows(members, &card, addr, &order).await;
    assert_eq!(members.len(), 2);
    text_flows(&mut members, &card).await;
}

#[tokio::test]
async fn personne_pour_reprendre_on_repasse_par_l_annuaire() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "seul");
    let (addr, host) = start_crashable_host(&card);

    let mut ana = member(addr, &card, "ana", Identity::generate()).await;
    // Bob ne peut pas héberger (il n'a rien proposé) : il n'est pas successeur.
    let station = Station::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let opts = Options {
        key: Some(card.access_key()),
        ..Default::default()
    };
    let bob_id = Identity::generate();
    let (bob, mut bob_events) = Client::connect_with(station.endpoint.clone(), addr, &bob_id, "bob", &opts)
        .await
        .unwrap();
    let order = succession(&mut ana, 1).await;
    wait_for(
        &mut bob_events,
        |e| matches!(e, Event::Succession(c) if c.iter().any(|c| c.heir)),
    )
    .await;

    host.close(0u32.into(), b"panne");
    crash(ana);
    let p = Plan {
        key: card.access_key(),
        old_host: addr,
        old_cert: bob.host_cert(),
        contacts: order,
        graceful: false,
        me: bob.peer_id,
    };
    let outcome = failover::follow(&p, &station.endpoint, &bob_id, "bob", |_| {}).await;
    assert!(matches!(outcome, Outcome::Lost));
}

#[tokio::test]
async fn fausse_alerte_l_hote_etait_toujours_la() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "fausse alerte");
    let (addr, _host) = start_host(&card);

    let mut ana = member(addr, &card, "ana", Identity::generate()).await;
    let mut bob = member(addr, &card, "bob", Identity::generate()).await;
    let order = succession(&mut ana, 2).await;
    succession(&mut bob, 2).await;

    // C'est la connexion du premier successeur qui a sauté, pas l'hôte :
    // l'hôte ne l'a pas encore remarqué quand il revient.
    let first = order.iter().find(|c| c.heir).unwrap().peer;
    let heir = if first == ana.client.peer_id { ana } else { bob };
    let before = heir.client.peer_id;
    let p = plan(&heir, &card, addr, order, false);
    let (client, _events, hosts) = follow(&heir, p, &card).await;
    assert!(!hosts, "il ne doit pas monter un second serveur");
    assert_eq!(client.peer_id, before, "il retrouve son numéro");
}
