//! Messages enrichis, modération et pièces jointes, en local : réponses,
//! réactions, modifications et suppressions ; historique recopié par un
//! membre ; expulsion et bannissement ; anti-spam ; fichiers.

use std::time::Duration;

use rip_discord::{
    client::{Client, Event, Refused, ServerView, TooFast},
    core::{
        card::{Kind, Message, ServerCard, SignedMessage, now_ms},
        files,
        identity::Identity,
        store::HistoryLog,
    },
    server::Host,
};
use tokio::{sync::mpsc::UnboundedReceiver, time::timeout};

async fn wait_for(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    loop {
        let ev = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("délai dépassé")
            .expect("canal fermé");
        if pred(&ev) {
            return ev;
        }
    }
}

/// Prochain message reçu, appliqué à la vue.
async fn receive(rx: &mut UnboundedReceiver<Event>, view: &mut ServerView) -> SignedMessage {
    let ev = wait_for(rx, |e| matches!(e, Event::Text { .. })).await;
    view.apply(&ev);
    match ev {
        Event::Text { msg, .. } => msg,
        _ => unreachable!(),
    }
}

fn host(card: &ServerCard) -> std::net::SocketAddr {
    let host = Host::bind(
        "127.0.0.1:0".parse().unwrap(),
        card.clone(),
        HistoryLog::in_memory(card.id, card.history()),
    )
    .unwrap();
    let addr = host.local_addr().unwrap();
    tokio::spawn(host.run());
    addr
}

/// Attend que l'hôte ait traité tout ce que ce client a envoyé avant.
async fn flush(client: &mut Client, rx: &mut UnboundedReceiver<Event>) {
    client.ping().unwrap();
    wait_for(rx, |e| matches!(e, Event::Pong { .. })).await;
}

#[tokio::test]
async fn repondre_reagir_modifier_supprimer() {
    let owner = Identity::generate();
    let bob_id = Identity::generate();
    let card = ServerCard::create(&owner, "messages");
    let general = card.settings().text_channels[0].id;
    let addr = host(&card);

    let (mut alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (bob, mut bob_rx) = Client::connect(addr, card.access_key(), &bob_id, "bob").await.unwrap();
    let mut alice_view = ServerView::new(&alice, "alice", None);
    let mut bob_view = ServerView::new(&bob, "bob", None);

    let first = alice.send_text(general, "salut").unwrap();
    alice_view.push_own(first.clone());
    assert_eq!(receive(&mut bob_rx, &mut bob_view).await, first);

    // Bob répond en mentionnant Alice : elle est notifiée, pas lui.
    let reply = bob
        .send_message(
            general,
            "@alice coucou",
            Kind::Rich {
                reply_to: Some(first.id()),
                mentions: vec![owner.id()],
                files: vec![],
            },
        )
        .unwrap();
    bob_view.push_own(reply.clone());
    let got = receive(&mut alice_rx, &mut alice_view).await;
    assert_eq!(got, reply);
    assert!(alice_view.mentions_me(&got));
    assert!(!bob_view.mentions_me(&reply));
    assert_eq!(bob_view.message(&first.id()), Some(&first));

    // Deux 👍 sur le premier message, dont un retiré puis remis.
    let react = |on| Kind::React {
        target: first.id(),
        emoji: "👍".into(),
        on,
    };
    for on in [true, false, true] {
        let m = bob.send_message(general, "", react(on)).unwrap();
        bob_view.push_own(m);
        receive(&mut alice_rx, &mut alice_view).await;
    }
    let m = alice.send_message(general, "", react(true)).unwrap();
    alice_view.push_own(m);
    receive(&mut bob_rx, &mut bob_view).await;
    assert_eq!(bob_view.reactions(&first), [("👍".to_string(), 2, true)]);
    assert_eq!(alice_view.reactions(&first), [("👍".to_string(), 2, true)]);

    // Alice corrige son message ; Bob ne peut pas le modifier à sa place.
    let edit = alice
        .send_message(general, "salut à tous", Kind::Edit { target: first.id() })
        .unwrap();
    alice_view.push_own(edit);
    receive(&mut bob_rx, &mut bob_view).await;
    let forged = bob
        .send_message(general, "pirate", Kind::Edit { target: first.id() })
        .unwrap();
    receive(&mut alice_rx, &mut alice_view).await;
    bob_view.push_own(forged);
    for view in [&alice_view, &bob_view] {
        assert_eq!(view.text(&first), "salut à tous");
        assert!(view.edited(&first));
    }

    // Bob, simple membre, ne peut pas supprimer le message d'Alice…
    let denied = bob
        .send_message(general, "", Kind::Delete { target: first.id() })
        .unwrap();
    bob_view.push_own(denied);
    receive(&mut alice_rx, &mut alice_view).await;
    assert!(!alice_view.is_deleted(&first));
    assert!(!bob_view.can_delete(&first));

    // … Alice, créatrice du serveur, peut supprimer celui de Bob.
    assert!(alice_view.can_delete(&reply));
    let delete = alice
        .send_message(general, "", Kind::Delete { target: reply.id() })
        .unwrap();
    alice_view.push_own(delete);
    receive(&mut bob_rx, &mut bob_view).await;
    for view in [&alice_view, &bob_view] {
        let left: Vec<_> = view.messages(general).map(|m| m.id()).collect();
        assert_eq!(left, [first.id()]);
    }

    // Un arrivant ne reçoit plus du tout le message supprimé.
    flush(&mut alice, &mut alice_rx).await;
    let (carol, mut carol_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "carol")
        .await
        .unwrap();
    let mut carol_view = ServerView::new(&carol, "carol", None);
    loop {
        let ev = wait_for(&mut carol_rx, |e| matches!(e, Event::History(_) | Event::HistoryEnd)).await;
        if let Event::History(h) = &ev {
            assert!(h.iter().all(|m| m.id() != reply.id() && m.body.body != "@alice coucou"));
        }
        carol_view.apply(&ev);
        if ev == Event::HistoryEnd {
            break;
        }
    }
    let left: Vec<_> = carol_view
        .messages(general)
        .map(|m| carol_view.text(m).to_string())
        .collect();
    assert_eq!(left, ["salut à tous"]);
    assert_eq!(carol_view.reactions(&first), [("👍".to_string(), 2, false)]);
}

#[tokio::test]
async fn historique_d_un_membre_repris_par_l_hote() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "copie");
    let general = card.settings().text_channels[0].id;
    // L'hôte part de zéro (il vient de reprendre le serveur, par exemple).
    let addr = host(&card);

    let (_bob, mut bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();
    let (mut alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    wait_for(&mut alice_rx, |e| *e == Event::HistoryEnd).await;

    // Écrits pendant que cet hôte n'était pas là, gardés par Alice (une
    // milliseconde d'écart chacun, comme les envoie un client).
    let start = now_ms() - 60_000;
    let n = std::cell::Cell::new(0);
    let old = |channel, body: &str| {
        n.set(n.get() + 1);
        Message {
            server: card.id,
            channel,
            author_name: "alice".into(),
            at_ms: start + n.get(),
            body: body.into(),
            kind: Kind::Text,
        }
        .sign(&owner)
    };
    let kept = vec![old(general, "un"), old(general, "deux")];
    let mut tampered = old(general, "trois");
    tampered.body.body = "trafiqué".into();
    let mut sent = kept.clone();
    sent.extend([old(999, "salon inconnu"), tampered]);
    alice.backfill(sent).unwrap();

    // Bob, déjà là, reçoit ce qui manquait, sans ce qui est invalide.
    match wait_for(&mut bob_rx, |e| matches!(e, Event::History(_))).await {
        Event::History(h) => assert_eq!(h, kept),
        _ => unreachable!(),
    }
    // Renvoyer la même copie ne rediffuse rien.
    alice.backfill(kept.clone()).unwrap();
    flush(&mut alice, &mut alice_rx).await;

    // Et les arrivants l'ont dans l'historique.
    let (_carol, mut carol_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "carol")
        .await
        .unwrap();
    match wait_for(&mut carol_rx, |e| matches!(e, Event::History(_))).await {
        Event::History(h) => assert_eq!(h, kept),
        _ => unreachable!(),
    }
    let next = timeout(Duration::from_millis(300), async {
        wait_for(&mut bob_rx, |e| matches!(e, Event::History(_))).await
    });
    assert!(next.await.is_err(), "rien de neuf à rediffuser");
}

#[tokio::test]
async fn expulser_puis_bannir() {
    let owner = Identity::generate();
    let bob_id = Identity::generate();
    let mut card = ServerCard::create(&owner, "modération");
    let general = card.settings().text_channels[0].id;
    let addr = host(&card);

    let (mut alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (bob, mut bob_rx) = Client::connect(addr, card.access_key(), &bob_id, "bob").await.unwrap();
    let (mut carol, mut carol_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "carol")
        .await
        .unwrap();

    // Carol n'est pas admin : sa demande est ignorée, Bob reste.
    carol.kick(bob.peer_id).unwrap();
    flush(&mut carol, &mut carol_rx).await;
    bob.send_text(general, "toujours là").unwrap();
    wait_for(&mut alice_rx, |e| matches!(e, Event::Text { .. })).await;

    // Alice l'expulse : il est prévenu et déconnecté, mais peut revenir.
    alice.kick(bob.peer_id).unwrap();
    wait_for(&mut bob_rx, |e| matches!(e, Event::Kicked { .. })).await;
    let gone = bob.peer_id;
    wait_for(
        &mut alice_rx,
        |e| matches!(e, Event::PeerLeft { peer_id } if *peer_id == gone),
    )
    .await;
    let (bob, mut bob_rx) = Client::connect(addr, card.access_key(), &bob_id, "bob").await.unwrap();

    // Banni : déconnecté tout de suite, et refusé ensuite.
    card.set_banned(&owner, bob_id.id(), true).unwrap();
    alice.update_card(&card).unwrap();
    wait_for(&mut bob_rx, |e| matches!(e, Event::Kicked { .. })).await;
    let err = match Client::connect(addr, card.access_key(), &bob_id, "bob").await {
        Ok(_) => panic!("un banni ne doit pas pouvoir revenir"),
        Err(e) => e,
    };
    assert!(err.downcast_ref::<Refused>().is_some(), "{err:#}");
    drop(bob);

    // Ses messages relayés par un autre ne passent pas non plus.
    let from_bob = Message {
        server: card.id,
        channel: general,
        author_name: "bob".into(),
        at_ms: now_ms(),
        body: "je reviens par la fenêtre".into(),
        kind: Kind::Text,
    }
    .sign(&bob_id);
    carol.backfill(vec![from_bob]).unwrap();
    flush(&mut carol, &mut carol_rx).await;
    let next = timeout(Duration::from_millis(300), async {
        wait_for(&mut alice_rx, |e| matches!(e, Event::History(_))).await
    });
    assert!(next.await.is_err());

    // Débanni : il revient.
    card.set_banned(&owner, bob_id.id(), false).unwrap();
    alice.update_card(&card).unwrap();
    flush(&mut alice, &mut alice_rx).await;
    Client::connect(addr, card.access_key(), &bob_id, "bob").await.unwrap();
}

#[tokio::test]
async fn trop_de_messages_d_un_coup() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "spam");
    let general = card.settings().text_channels[0].id;
    let addr = host(&card);
    let (alice, _alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (_bob, mut bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();

    let results: Vec<_> = (0..10).map(|i| alice.send_text(general, format!("spam {i}"))).collect();
    let sent = results.iter().take_while(|r| r.is_ok()).count();
    assert!((3..10).contains(&sent), "{sent} messages passés");
    let err = results[sent].as_ref().unwrap_err();
    let wait = err.downcast_ref::<TooFast>().expect("TooFast").0;
    assert!(wait > Duration::ZERO && wait < Duration::from_secs(5));

    // Ce qui est passé arrive ; les réactions ont leur propre réserve.
    for _ in 0..sent {
        wait_for(&mut bob_rx, |e| matches!(e, Event::Text { .. })).await;
    }
    let target = match &results[0] {
        Ok(m) => m.id(),
        Err(_) => unreachable!(),
    };
    alice
        .send_message(
            general,
            "",
            Kind::React {
                target,
                emoji: "🔥".into(),
                on: true,
            },
        )
        .unwrap();
}

#[tokio::test]
async fn pieces_jointes_et_relais_par_un_membre() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "fichiers");
    let addr = host(&card);
    let (alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (bob, _bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();

    // Envoyé par Alice, téléchargé par Bob.
    let photo = vec![7u8; 300_000];
    alice.files().put(&photo).await.unwrap();
    assert_eq!(bob.files().get(files::hash(&photo)).await.unwrap(), photo);

    // L'hôte n'a pas ce fichier (envoyé à un ancien hôte) : il le demande
    // aux membres, Alice l'a gardé et le renvoie.
    let old = b"envoye avant la releve".to_vec();
    let hash = files::hash(&old);
    let download = tokio::spawn(async move { bob.files().get(hash).await });
    match wait_for(&mut alice_rx, |e| matches!(e, Event::FileWanted { .. })).await {
        Event::FileWanted { hash: wanted } => assert_eq!(wanted, hash),
        _ => unreachable!(),
    }
    alice.files().put(&old).await.unwrap();
    assert_eq!(download.await.unwrap().unwrap(), old);
}
