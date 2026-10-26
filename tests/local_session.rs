//! Hôte + clients en local : texte, historique, salons vocaux, réglages
//! signés et ré-hébergement du même serveur par un autre membre.

use std::time::Duration;

use bytes::Bytes;
use rip_discord::{
    client::{Client, Event, ServerView},
    core::{
        card::{HistoryPolicy, ServerCard},
        identity::Identity,
        protocol::{Control, PeerId},
        screen,
        store::{HistoryLog, Store},
        voice,
    },
    server::Host,
};
use tokio::{sync::mpsc::UnboundedReceiver, time::timeout};

/// Prochain événement, sans l'ordre de succession (renvoyé à chaque arrivée
/// ou départ) ni la fin de l'historique : ils n'intéressent pas ces tests.
async fn next(rx: &mut UnboundedReceiver<Event>) -> Event {
    loop {
        let ev = timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("délai dépassé")
            .expect("canal fermé");
        if !matches!(ev, Event::Succession(_) | Event::HistoryEnd) {
            return ev;
        }
    }
}

/// Attend le premier événement qui satisfait `pred`, en ignorant les autres.
async fn wait_for(rx: &mut UnboundedReceiver<Event>, pred: impl Fn(&Event) -> bool) -> Event {
    loop {
        let ev = next(rx).await;
        if pred(&ev) {
            return ev;
        }
    }
}

fn host(card: &ServerCard, history: HistoryLog) -> std::net::SocketAddr {
    let host = Host::bind("127.0.0.1:0".parse().unwrap(), card.clone(), history).unwrap();
    let addr = host.local_addr().unwrap();
    tokio::spawn(host.run());
    addr
}

fn temp_store(tag: &str) -> Store {
    let dir = std::env::temp_dir().join(format!("rip-discord-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Store::at(dir).unwrap()
}

#[tokio::test]
async fn texte_et_ping_pong() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "test");
    let general = card.settings().text_channels[0].id;
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));

    let (mut alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (bob, mut bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();
    assert_eq!(bob.initial_peers.len(), 1);
    assert_eq!(bob.card, card);

    match next(&mut alice_rx).await {
        Event::PeerJoined(p) => assert_eq!(p.name, "bob"),
        other => panic!("{other:?}"),
    }

    let sent = alice.send_text(general, "salut bob").unwrap();
    assert_eq!(
        next(&mut bob_rx).await,
        Event::Text {
            from: alice.peer_id,
            msg: sent
        }
    );

    alice.ping().unwrap();
    assert!(matches!(next(&mut alice_rx).await, Event::Pong { .. }));

    bob.quit().await;
    assert_eq!(next(&mut alice_rx).await, Event::PeerLeft { peer_id: 2 });
}

#[tokio::test]
async fn historique_servi_aux_arrivants_selon_la_politique() {
    let owner = Identity::generate();
    let mut card = ServerCard::create(&owner, "test");
    let general = card.settings().text_channels[0].id;
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));

    let (mut alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    alice.send_text(general, "un").unwrap();
    alice.send_text(general, "deux").unwrap();
    alice.ping().unwrap();
    wait_for(&mut alice_rx, |e| matches!(e, Event::Pong { .. })).await;

    let (_bob, mut bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();
    match next(&mut bob_rx).await {
        Event::History(h) => {
            let bodies: Vec<_> = h.iter().map(|m| m.body.body.as_str()).collect();
            assert_eq!(bodies, ["un", "deux"]);
        }
        other => panic!("{other:?}"),
    }

    // Le créateur coupe l'historique : l'hôte efface tout et le dit à tous.
    card.edit(&owner, |s| {
        s.history = HistoryPolicy {
            keep: false,
            max_per_channel: 0,
        }
    })
    .unwrap();
    alice.update_card(&card).unwrap();
    match wait_for(&mut bob_rx, |e| matches!(e, Event::CardUpdated(_))).await {
        Event::CardUpdated(c) => assert!(!c.history().keep),
        _ => unreachable!(),
    }
    let (_carol, mut carol_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "carol")
        .await
        .unwrap();
    // Pas d'historique : le premier événement de carol n'en est pas un.
    alice.send_text(general, "trois").unwrap();
    match next(&mut carol_rx).await {
        Event::Text { msg, .. } => assert_eq!(msg.body.body, "trois"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn un_membre_ne_peut_pas_modifier_le_serveur() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "test");
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));

    let intrus = Identity::generate();
    let (_alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (mallory, _rx) = Client::connect(addr, card.access_key(), &intrus, "mallory")
        .await
        .unwrap();
    wait_for(&mut alice_rx, |e| matches!(e, Event::PeerJoined(_))).await;

    let mut forged = card.clone();
    assert!(forged.edit(&intrus, |s| s.name = "piraté".into()).is_err());
    // Même une carte mal signée envoyée à la main est ignorée par l'hôte.
    forged.settings.body.name = "piraté".into();
    forged.settings.body.revision += 1;
    mallory.update_card(&forged).unwrap();

    let (_bob, _) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();
    let (late, _) = Client::connect(addr, card.access_key(), &Identity::generate(), "late")
        .await
        .unwrap();
    assert_eq!(late.card.name(), "test");
}

#[tokio::test]
async fn salons_vocaux_hote_parmi_les_participants() {
    let owner = Identity::generate();
    let mut card = ServerCard::create(&owner, "test");
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));
    let vocal = card.settings().voice_channels[0].id;

    let (alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (bob, mut bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();

    // Second salon vocal créé par le créateur.
    card.edit(&owner, |s| {
        s.add_voice_channel("Jeux");
    })
    .unwrap();
    let jeux = card.settings().voice_channels[1].id;
    alice.update_card(&card).unwrap();
    wait_for(&mut bob_rx, |e| matches!(e, Event::CardUpdated(_))).await;

    bob.join_voice(vocal).unwrap();
    let room = |e: &Event| match e {
        Event::Voice(r) => Some(r.clone()),
        _ => None,
    };
    let r = room(&wait_for(&mut alice_rx, |e| matches!(e, Event::Voice(_))).await).unwrap();
    assert_eq!(
        (r.channel, r.members.clone(), r.host),
        (vocal, vec![bob.peer_id], Some(bob.peer_id))
    );

    alice.join_voice(vocal).unwrap();
    let r = room(&wait_for(&mut alice_rx, |e| matches!(e, Event::Voice(_))).await).unwrap();
    assert_eq!(r.members, [bob.peer_id, alice.peer_id]);
    assert_eq!(r.host, Some(bob.peer_id));

    // Bob change de salon : alice devient l'hôte du premier, bob celui de « Jeux ».
    bob.join_voice(jeux).unwrap();
    let r = room(&wait_for(&mut alice_rx, |e| matches!(e, Event::Voice(_))).await).unwrap();
    assert_eq!((r.channel, r.host), (vocal, Some(alice.peer_id)));
    let r = room(&wait_for(&mut alice_rx, |e| matches!(e, Event::Voice(_))).await).unwrap();
    assert_eq!((r.channel, r.host), (jeux, Some(bob.peer_id)));

    // Départ d'alice : le salon se vide.
    alice.quit().await;
    let r = room(
        &wait_for(
            &mut bob_rx,
            |e| matches!(e, Event::Voice(r) if r.channel == vocal && r.members.is_empty()),
        )
        .await,
    )
    .unwrap();
    assert!(r.members.is_empty() && r.host.is_none());
}

/// Bob rejoint, garde sa copie, puis héberge le serveur alors que le créateur
/// est parti : même serveur, mêmes salons, même historique.
#[tokio::test]
async fn un_membre_re_heberge_le_serveur() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "Les potes");
    let general = card.settings().text_channels[0].id;
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));

    let bob_store = temp_store("bob");
    let bob_id = bob_store.identity().unwrap();
    let (alice, _alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (bob, mut bob_rx) = Client::connect(addr, card.access_key(), &bob_id, "bob").await.unwrap();
    let mut view = ServerView::new(&bob, "bob", Some(bob_store.clone()));
    alice.send_text(general, "message d'alice").unwrap();
    let ev = wait_for(&mut bob_rx, |e| matches!(e, Event::Text { .. })).await;
    view.apply(&ev);
    view.push_own(bob.send_text(general, "réponse de bob").unwrap());
    drop(view);
    bob.quit().await;
    alice.quit().await;

    // Le créateur n'est plus là : bob ré-héberge sa copie.
    let saved = bob_store.servers();
    assert_eq!(saved.len(), 1);
    let saved = saved.into_iter().next().unwrap();
    assert_eq!(saved.id, card.id);
    let addr2 = host(&saved, bob_store.history(&saved));

    let (carol, mut carol_rx) = Client::connect(addr2, card.access_key(), &Identity::generate(), "carol")
        .await
        .unwrap();
    assert_eq!(carol.card.id, card.id);
    assert_eq!(carol.card.owner, owner.id());
    match next(&mut carol_rx).await {
        Event::History(h) => {
            let bodies: Vec<_> = h.iter().map(|m| m.body.body.as_str()).collect();
            assert_eq!(bodies, ["message d'alice", "réponse de bob"]);
            assert_eq!(h[0].author(), owner.id());
        }
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(bob_store.root());
}

/// La voix va aux autres membres du salon vocal, et à eux seuls.
#[tokio::test]
async fn voix_relayee_dans_le_salon_seulement() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "test");
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));
    let vocal = card.settings().voice_channels[0].id;

    let (alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (mut bob, mut bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();
    let (mut carol, _carol_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "carol")
        .await
        .unwrap();
    let mut bob_voice = bob.take_voice().unwrap();
    let mut carol_voice = carol.take_voice().unwrap();

    alice.join_voice(vocal).unwrap();
    bob.join_voice(vocal).unwrap();
    for rx in [&mut alice_rx, &mut bob_rx] {
        wait_for(rx, |e| matches!(e, Event::Voice(r) if r.members.len() == 2)).await;
    }

    // Les datagrammes peuvent se perdre : on en envoie plusieurs.
    for seq in 0..5 {
        alice.send_voice(seq, b"trame opus", voice::Route::Room);
    }
    let got = timeout(Duration::from_secs(5), bob_voice.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (got.from, got.opus.as_slice()),
        (alice.peer_id, b"trame opus".as_slice())
    );
    assert!(timeout(Duration::from_millis(300), carol_voice.recv()).await.is_err());
}

/// Un écran partagé n'arrive qu'à ceux qui le regardent, dans le même salon,
/// en commençant par une image clé ; son son aussi.
#[tokio::test]
async fn partage_d_ecran_aux_spectateurs_seulement() {
    async fn next_image(rx: &mut UnboundedReceiver<(PeerId, screen::Frame)>) -> (PeerId, screen::Frame) {
        timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap()
    }
    let frame = |seq, key| screen::Frame {
        seq,
        key,
        camera: false,
        data: Bytes::from(format!("image {seq}")),
    };

    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "test");
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));
    let vocal = card.settings().voice_channels[0].id;

    let (alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (mut bob, mut bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();
    let (mut carol, mut carol_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "carol")
        .await
        .unwrap();
    let mut bob_screens = bob.take_screens().unwrap();
    let mut carol_screens = carol.take_screens().unwrap();
    let mut bob_sounds = bob.take_sounds().unwrap();
    let mut carol_sounds = carol.take_sounds().unwrap();

    for c in [&alice, &bob, &carol] {
        c.join_voice(vocal).unwrap();
    }
    wait_for(&mut alice_rx, |e| matches!(e, Event::Voice(r) if r.members.len() == 3)).await;
    alice.share_screen(true).unwrap();
    for rx in [&mut bob_rx, &mut carol_rx] {
        wait_for(rx, |e| matches!(e, Event::Voice(r) if r.sharing == [alice.peer_id])).await;
    }
    // Bob arrive : l'hôte demande une image clé à Alice pour lui.
    bob.watch_screen(Some(alice.peer_id)).unwrap();
    let bob_id = bob.peer_id;
    let key_for_bob = move |e: &Event| matches!(e, Event::Control(Control::ScreenKey(p)) if *p == bob_id);
    wait_for(&mut alice_rx, key_for_bob).await;

    // Rien ne part vers l'hôte avant une image clé : il en faut une.
    assert!(alice.send_screen(&frame(0, false)));
    assert!(!alice.send_screen(&frame(1, true)));
    assert_eq!(next_image(&mut bob_screens).await, (alice.peer_id, frame(1, true)));
    assert!(!alice.send_screen(&frame(2, false)));
    assert_eq!(next_image(&mut bob_screens).await, (alice.peer_id, frame(2, false)));
    assert!(timeout(Duration::from_millis(300), carol_screens.recv()).await.is_err());

    // Le son du partage, aux seuls spectateurs.
    alice.send_sound(7, b"son opus");
    let s = timeout(Duration::from_secs(5), bob_sounds.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (s.from, s.seq, s.opus.as_slice()),
        (alice.peer_id, 7, b"son opus".as_slice())
    );
    assert!(timeout(Duration::from_millis(300), carol_sounds.recv()).await.is_err());

    // Bob a perdu le fil : la demande arrive à Alice, et rien ne lui est
    // relayé avant la prochaine image clé.
    bob.request_key(alice.peer_id).unwrap();
    wait_for(&mut alice_rx, key_for_bob).await;
    alice.send_screen(&frame(3, false));
    assert!(timeout(Duration::from_millis(300), bob_screens.recv()).await.is_err());
    alice.send_screen(&frame(4, true));
    assert_eq!(next_image(&mut bob_screens).await, (alice.peer_id, frame(4, true)));

    alice.share_screen(false).unwrap();
    wait_for(&mut bob_rx, |e| matches!(e, Event::Voice(r) if r.sharing.is_empty())).await;
}

/// Webcam relayée par l'hôte, pour ceux qui ne joignent pas son propriétaire
/// directement : seulement à ceux qui la demandent, dans le même salon.
#[tokio::test]
async fn webcam_relayee_a_qui_la_demande() {
    async fn next_image(rx: &mut UnboundedReceiver<(PeerId, screen::Frame)>) -> (PeerId, screen::Frame) {
        timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap()
    }
    let frame = |seq, key| screen::Frame {
        seq,
        key,
        camera: true,
        data: Bytes::from(format!("visage {seq}")),
    };

    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "test");
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));
    let vocal = card.settings().voice_channels[0].id;

    let (alice, mut alice_rx) = Client::connect(addr, card.access_key(), &owner, "alice").await.unwrap();
    let (mut bob, mut bob_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "bob")
        .await
        .unwrap();
    let (mut carol, mut carol_rx) = Client::connect(addr, card.access_key(), &Identity::generate(), "carol")
        .await
        .unwrap();
    let mut bob_screens = bob.take_screens().unwrap();
    let mut carol_screens = carol.take_screens().unwrap();

    for c in [&alice, &bob, &carol] {
        c.join_voice(vocal).unwrap();
    }
    wait_for(&mut alice_rx, |e| matches!(e, Event::Voice(r) if r.members.len() == 3)).await;
    alice.share_camera(true).unwrap();
    for rx in [&mut bob_rx, &mut carol_rx] {
        wait_for(rx, |e| matches!(e, Event::Voice(r) if r.cameras == [alice.peer_id])).await;
    }

    // Bob ne joint pas Alice directement : l'hôte lui relaie sa webcam,
    // en commençant par une image clé demandée à Alice.
    bob.relay_cameras(vec![alice.peer_id]).unwrap();
    let bob_id = bob.peer_id;
    let key_for_bob = move |e: &Event| matches!(e, Event::Control(Control::CameraKey(p)) if *p == bob_id);
    wait_for(&mut alice_rx, key_for_bob).await;
    assert!(alice.send_camera(&frame(0, false)));
    assert!(!alice.send_camera(&frame(1, true)));
    assert_eq!(next_image(&mut bob_screens).await, (alice.peer_id, frame(1, true)));
    assert!(!alice.send_camera(&frame(2, false)));
    assert_eq!(next_image(&mut bob_screens).await, (alice.peer_id, frame(2, false)));
    // Carol la reçoit directement : l'hôte ne lui envoie rien.
    assert!(timeout(Duration::from_millis(300), carol_screens.recv()).await.is_err());

    // Bob a perdu le fil : rien avant la prochaine image clé.
    bob.request_camera_key(alice.peer_id).unwrap();
    wait_for(&mut alice_rx, key_for_bob).await;
    alice.send_camera(&frame(3, false));
    assert!(timeout(Duration::from_millis(300), bob_screens.recv()).await.is_err());
    alice.send_camera(&frame(4, true));
    assert_eq!(next_image(&mut bob_screens).await, (alice.peer_id, frame(4, true)));

    // Webcam éteinte : plus rien n'est relayé, même rallumée sans nouvelle demande.
    alice.share_camera(false).unwrap();
    wait_for(&mut bob_rx, |e| matches!(e, Event::Voice(r) if r.cameras.is_empty())).await;
    alice.share_camera(true).unwrap();
    wait_for(
        &mut bob_rx,
        |e| matches!(e, Event::Voice(r) if r.cameras == [alice.peer_id]),
    )
    .await;
    alice.send_camera(&frame(5, true));
    assert!(timeout(Duration::from_millis(300), bob_screens.recv()).await.is_err());
}

/// Sans le code (la clé d'accès), on n'entre pas : rien n'est envoyé, ni
/// l'historique ni la liste des membres.
#[tokio::test]
async fn sans_le_code_on_n_entre_pas() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "privé");
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));

    let intrus = Identity::generate();
    let err = Client::connect(addr, [0; 16], &intrus, "scanner").await.err().unwrap();
    assert!(format!("{err:#}").contains("Code d'invitation incorrect"), "{err:#}");

    // Le bon code passe.
    Client::connect(addr, card.access_key(), &intrus, "ami").await.unwrap();
}

/// Un hôte qui ne connaît pas la clé (fausse adresse dans l'annuaire, autre
/// serveur) est démasqué par le membre, même s'il l'accepte.
#[tokio::test]
async fn un_faux_hote_est_demasque() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "vrai");
    let imposteur = Identity::generate();
    let faux = ServerCard::create(&imposteur, "faux");
    let addr = host(&faux, HistoryLog::in_memory(faux.id, faux.history()));

    // Même en connaissant la clé du vrai serveur, l'imposteur ne peut pas la
    // prouver : son hôte vérifie la sienne et refuse, ou le membre refuse l'hôte.
    let err = Client::connect(addr, card.access_key(), &owner, "alice")
        .await
        .err()
        .unwrap();
    assert!(
        format!("{err:#}").contains("Code d'invitation incorrect") || format!("{err:#}").contains("ne connaît pas"),
        "{err:#}"
    );
}

/// Un admin change le code : l'ancien ne marche plus, les membres connectés
/// reçoivent le nouveau.
#[tokio::test]
async fn changer_le_code_ferme_la_porte() {
    let owner = Identity::generate();
    let card = ServerCard::create(&owner, "test");
    let old = card.access_key();
    let addr = host(&card, HistoryLog::in_memory(card.id, card.history()));

    let (alice, _alice_rx) = Client::connect(addr, old, &owner, "alice").await.unwrap();
    let (_bob, mut bob_rx) = Client::connect(addr, old, &Identity::generate(), "bob").await.unwrap();

    let mut rotated = card.clone();
    rotated.new_access_key(&owner).unwrap();
    let new = rotated.access_key();
    assert_ne!(new, old);
    alice.update_card(&rotated).unwrap();
    match wait_for(&mut bob_rx, |e| matches!(e, Event::CardUpdated(_))).await {
        Event::CardUpdated(c) => assert_eq!(c.access_key(), new),
        _ => unreachable!(),
    }

    let err = Client::connect(addr, old, &Identity::generate(), "ex")
        .await
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("Code d'invitation incorrect"), "{err:#}");
    Client::connect(addr, new, &Identity::generate(), "nouveau")
        .await
        .unwrap();
}
