//! Vocal direct entre participants, sans passer par l'hôte du serveur : la
//! voix part de pair à pair, un chuchotement n'arrive qu'à ses destinataires,
//! un écran partagé qu'à ceux qui le regardent, une webcam à tout le salon.

use std::{net::SocketAddr, time::Duration};

use bytes::Bytes;
use rip_discord::{
    client::{
        mesh::{Incoming, Mesh},
        station::Station,
    },
    core::{
        card::ServerCard,
        identity::Identity,
        protocol::{Contact, PeerId},
        screen,
    },
};
use tokio::{
    sync::mpsc::UnboundedReceiver,
    time::{sleep, timeout},
};

struct Participant {
    peer: PeerId,
    mesh: Mesh,
    incoming: Incoming,
    contact: Contact,
}

fn participant(peer: PeerId) -> Participant {
    let identity = Identity::generate();
    let station = Station::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let contact = Contact {
        peer,
        user: identity.id(),
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], station.port()))],
        cert: station.cert.fingerprint,
        heir: false,
    };
    let (mesh, incoming) = Mesh::start(station, identity);
    Participant {
        peer,
        mesh,
        incoming,
        contact,
    }
}

/// Trois participants (1, 2, 3) dans le même salon, tous reliés entre eux.
async fn linked_room() -> (Vec<Participant>, Vec<PeerId>) {
    let card = ServerCard::create(&Identity::generate(), "vocal");
    let room: Vec<Participant> = (1..=3).map(participant).collect();
    let contacts: Vec<Contact> = room.iter().map(|p| p.contact.clone()).collect();
    let peers: Vec<PeerId> = room.iter().map(|p| p.peer).collect();
    for p in &room {
        p.mesh.set_session(card.id, p.peer);
        p.mesh.set_contacts(&contacts);
        p.mesh.set_room(&peers);
    }
    timeout(Duration::from_secs(10), async {
        while !room.iter().all(|p| p.mesh.all_linked()) {
            sleep(Duration::from_millis(50)).await;
            room.iter().for_each(|p| p.mesh.tick());
        }
    })
    .await
    .expect("les participants ne se sont pas reliés");
    (room, peers)
}

#[tokio::test]
async fn voix_et_chuchotement_de_pair_a_pair() {
    let (mut room, peers) = linked_room().await;

    // Tout le salon entend.
    assert!(room[0].mesh.send_voice(7, b"salut", None).is_empty());
    for p in &mut room[1..] {
        let v = timeout(Duration::from_secs(5), p.incoming.voice.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (v.from, v.seq, v.opus.as_slice(), v.whisper),
            (1, 7, &b"salut"[..], false)
        );
    }

    // Chuchotement à 2 seulement : 3 n'entend rien.
    assert!(room[0].mesh.send_voice(8, b"psst", Some(&[2])).is_empty());
    let v = timeout(Duration::from_secs(5), room[1].incoming.voice.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((v.from, v.seq, v.whisper), (1, 8, true));
    assert!(
        timeout(Duration::from_millis(300), room[2].incoming.voice.recv())
            .await
            .is_err()
    );

    // 3 quitte le salon : sa connexion se ferme, les autres restent reliés.
    for p in &room[..2] {
        p.mesh.set_room(&peers[..2]);
    }
    assert!(room[0].mesh.all_linked() && room[1].mesh.all_linked());
    assert!(!room[0].mesh.linked(3));
}

/// Écran partagé de pair à pair : seul le spectateur le reçoit, en commençant
/// par une image clé, avec le son ; ses demandes d'image clé arrivent au partageur.
#[tokio::test]
async fn ecran_et_son_de_pair_a_pair() {
    async fn next_image(p: &mut Participant) -> (PeerId, screen::Frame) {
        timeout(Duration::from_secs(5), p.incoming.screens.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn nothing<T>(rx: &mut UnboundedReceiver<T>) -> bool {
        timeout(Duration::from_millis(300), rx.recv()).await.is_err()
    }
    async fn key_asked(p: &mut Participant) -> PeerId {
        timeout(Duration::from_secs(5), p.incoming.keys.recv())
            .await
            .unwrap()
            .unwrap()
    }
    let frame = |seq, key| screen::Frame {
        seq,
        key,
        camera: false,
        data: Bytes::from(format!("image {seq}")),
    };
    let (mut room, _) = linked_room().await;

    // 2 regarde l'écran de 1 : 1 doit lui envoyer une image clé.
    assert!(room[1].mesh.watch(1, true));
    assert_eq!(key_asked(&mut room[0]).await, 2);
    assert!(!room[0].mesh.send_screen(&frame(0, false)));
    assert!(!room[0].mesh.send_screen(&frame(1, true)));
    assert_eq!(next_image(&mut room[1]).await, (1, frame(1, true)));
    assert!(!room[0].mesh.send_screen(&frame(2, false)));
    assert_eq!(next_image(&mut room[1]).await, (1, frame(2, false)));
    assert!(nothing(&mut room[2].incoming.screens).await);

    // Le son du partage, au seul spectateur.
    room[0].mesh.send_sound(5, b"son");
    let s = timeout(Duration::from_secs(5), room[1].incoming.sounds.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((s.from, s.seq, s.opus.as_slice()), (1, 5, &b"son"[..]));
    assert!(nothing(&mut room[2].incoming.sounds).await);

    // 2 a perdu le fil : rien ne lui part avant la prochaine image clé.
    assert!(room[1].mesh.request_key(1));
    assert_eq!(key_asked(&mut room[0]).await, 2);
    room[0].mesh.send_screen(&frame(3, false));
    assert!(nothing(&mut room[1].incoming.screens).await);
    room[0].mesh.send_screen(&frame(4, true));
    assert_eq!(next_image(&mut room[1]).await, (1, frame(4, true)));

    // 3 demande une image clé sans regarder : ignoré.
    assert!(room[2].mesh.request_key(1));
    assert!(nothing(&mut room[0].incoming.keys).await);

    // 2 ne regarde plus : plus rien ne lui arrive.
    assert!(room[1].mesh.watch(1, false));
    sleep(Duration::from_millis(200)).await;
    room[0].mesh.send_screen(&frame(5, true));
    assert!(nothing(&mut room[1].incoming.screens).await);
}

/// Webcam de pair à pair : elle part vers tout le salon, sans qu'on la
/// demande, en commençant par une image clé ; un participant qui perd le fil
/// en redemande une.
#[tokio::test]
async fn webcam_a_tout_le_salon() {
    async fn next_image(p: &mut Participant) -> (PeerId, screen::Frame) {
        timeout(Duration::from_secs(5), p.incoming.screens.recv())
            .await
            .unwrap()
            .unwrap()
    }
    let frame = |seq, key| screen::Frame {
        seq,
        key,
        camera: true,
        data: Bytes::from(format!("visage {seq}")),
    };
    let (mut room, _) = linked_room().await;

    // Une image ordinaire ne sert à rien sans image clé : il en faut une.
    assert!(room[0].mesh.send_camera(&frame(0, false)));
    assert!(!room[0].mesh.send_camera(&frame(1, true)));
    for p in &mut room[1..] {
        assert_eq!(next_image(p).await, (1, frame(1, true)));
    }
    assert!(!room[0].mesh.send_camera(&frame(2, false)));
    for p in &mut room[1..] {
        assert_eq!(next_image(p).await, (1, frame(2, false)));
    }

    // 3 a perdu le fil : la demande arrive à 1, et 3 attend l'image clé
    // pendant que 2 continue de tout recevoir.
    assert!(room[2].mesh.request_camera_key(1));
    let asked = timeout(Duration::from_secs(5), room[0].incoming.camera_keys.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(asked, 3);
    // Pas confondue avec une demande pour l'écran.
    assert!(room[0].incoming.keys.try_recv().is_err());
    room[0].mesh.send_camera(&frame(3, false));
    assert_eq!(next_image(&mut room[1]).await, (1, frame(3, false)));
    assert!(
        timeout(Duration::from_millis(300), room[2].incoming.screens.recv())
            .await
            .is_err()
    );
    room[0].mesh.send_camera(&frame(4, true));
    assert_eq!(next_image(&mut room[2]).await, (1, frame(4, true)));
}

/// L'hôte du serveur change : les participants reviennent un à un chez le
/// nouveau, qui décrit d'abord un salon incomplet. La voix ne doit pas couper
/// entre ceux qui sont déjà revenus et ceux qui arrivent encore.
#[tokio::test]
async fn la_voix_tient_pendant_la_releve_de_l_hote() {
    let (mut room, peers) = linked_room().await;
    for p in &room {
        p.mesh.hold_room(Duration::from_millis(1500));
    }
    // Le nouvel hôte ne connaît encore que 1 et 2.
    for p in &room[..2] {
        p.mesh.set_room(&peers[..2]);
    }
    assert!(room[0].mesh.linked(3), "3 n'est pas encore revenu, mais reste relié");
    assert!(room[0].mesh.send_voice(1, b"tu m'entends ?", None).is_empty());
    let v = timeout(Duration::from_secs(5), room[2].incoming.voice.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((v.from, v.opus.as_slice()), (1, &b"tu m'entends ?"[..]));

    // 3 revient : rien n'a bougé.
    for p in &room {
        p.mesh.set_room(&peers);
    }
    assert!(room.iter().all(|p| p.mesh.all_linked()));

    // Passé le délai de grâce, un vrai départ coupe bien.
    sleep(Duration::from_millis(1600)).await;
    for p in &room[..2] {
        p.mesh.set_room(&peers[..2]);
    }
    assert!(!room[0].mesh.linked(3));
}
