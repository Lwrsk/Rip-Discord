# RIP Discord

[English](README.md) · **Français**

Chat local et privé façon Discord/TeamSpeak, en Rust. Un seul programme : chaque
instance peut créer un serveur, le rejoindre ou l'héberger à la place du
créateur. Pas de serveur central, pas de compte, tes données restent chez toi.

## Installer (Windows)

Télécharger `RIP-Discord-Setup-x.y.z.exe` dans les
[Releases](https://github.com/Lwrsk/Rip-Discord/releases) et le lancer. L'installeur
crée les raccourcis (menu Démarrer + bureau), autorise l'app dans le pare-feu
Windows et ajoute une entrée de désinstallation.

Mises à jour : l'app regarde la dernière Release de ce dépôt au lancement (puis
toutes les 6 h) et la propose dans un petit encart. « Mettre à jour » télécharge
l'installeur, vérifie sa taille et l'empreinte SHA-256 publiée par GitHub, puis
l'installe sans fenêtres et rouvre l'app (Windows demande l'accord
administrateur). Désactivable dans « Mes réglages > Données ».

## Utiliser

Au premier lancement, l'app demande juste un pseudo.

1. **Créer un serveur** : « Créer un serveur », un nom, et si l'historique est
   gardé. Rien d'autre à installer ni à configurer.
2. **Inviter** : bouton « Inviter » en haut du serveur. Le **code
   d'invitation** est caché par défaut (pour qu'il ne fuite pas sur un
   partage d'écran ou un stream) : « Copier le code » le copie sans
   l'afficher, « Afficher » le montre.
3. **Rejoindre** : l'ami clique « Rejoindre » sur l'accueil et colle le code
   (lui aussi caché pendant la saisie). C'est tout.
4. **Salons** : des salons **texte** et des salons **vocaux**, séparés, dans la
   colonne de gauche. Un point signale les nouveaux messages d'un salon, `@2`
   le nombre de fois où l'on t'a mentionné. Les messages sont groupés par
   auteur, avec l'heure et le jour.
   - **Répondre, réagir, modifier, supprimer** : survole un message (ou clic
     droit dessus). Flèche du haut dans la zone de saisie : modifier ton
     dernier message. Le créateur et les admins peuvent supprimer les
     messages des autres.
   - **Mentions** : tape `@` puis le début d'un pseudo (Tab complète) ;
     `@tous` prévient tout le monde. Une mention te notifie même si l'appli est
     en arrière-plan (notification Windows, la barre des tâches clignote, le
     nombre de messages non lus s'affiche dans le titre).
   - **Images et fichiers** : bouton « ＋ » ou glisser-déposer dans la fenêtre,
     jusqu'à 10 fichiers de 25 Mo par message. Les images s'affichent dans le
     fil ; un clic les enregistre dans Téléchargements (rien n'est ouvert
     automatiquement).
   - Deux personnes avec le même pseudo sont distinguées par un `#1a2b`.
   - Anti-spam : au-delà de quelques messages d'affilée, il faut attendre un
     instant.
5. **Vocal** : un clic sur un salon vocal pour y entrer, « Raccrocher » pour
   en sortir. En bas à gauche, « Micro » et « Son » coupent ton micro ou tout
   le son, même hors d'un salon. Clic droit sur le nom de quelqu'un dans le
   salon : son volume à lui. Le nom de celui qui parle s'allume en vert.
6. **Partage d'écran** : dans un salon vocal, « Partager l'écran… » ouvre le
   choix, avec un aperçu de chaque source : un **écran** entier ou une seule
   **fenêtre** (un jeu, un navigateur, un éditeur…). On y choisit aussi de
   **montrer ou non sa souris**, de **partager le son** (sous Windows : le son
   de la fenêtre partagée seulement, ou pour un écran entier tout le son du PC
   sauf les voix du salon) et la **qualité** (« Texte net » à 10 images/s,
   « Équilibré » à 20, « Fluide » à 30 pour les jeux). L'image part en vidéo
   H.264 : quelques mégabits par seconde. Pendant le partage, « Changer de source… » passe
   sur un autre écran ou une autre fenêtre **sans couper** les spectateurs, et
   la souris ou la qualité se changent à la volée. Si la fenêtre partagée est
   fermée, le partage s'arrête tout seul. Les autres participants voient
   « Regarder » à côté de ton nom ; seuls ceux qui cliquent reçoivent l'image,
   et « Plein écran » (ou double-clic sur l'image, Échap pour sortir) l'affiche
   en grand ; au-dessus de l'image, 🔊 coupe le son du partage et le curseur
   règle son volume. « Voir mon écran » / « Cacher mon écran » affiche ou cache
   l'aperçu de ce que les autres voient (pour toi seul, ton choix est retenu).
   Sous Windows, la capture passe par Windows.Graphics.Capture (comme OBS ou
   Discord) : une fenêtre reste visible même cachée derrière d'autres, et les
   jeux en fenêtré ou plein écran fenêtré passent. Un jeu en plein écran
   exclusif peut rester noir : passe-le en « plein écran fenêtré ».
7. **Webcam** : dans un salon vocal, « Activer la caméra » allume ta webcam ;
   tout le salon la voit en vignettes au-dessus des messages (la tienne en
   miroir), entourées de vert quand la personne parle, et 📷 s'affiche à côté
   des noms. L'image part en H.264, 640 pixels de large à 20 images/s, environ
   0,5 Mb/s par personne du salon, directement de PC à PC (par l'hôte du
   serveur seulement pour deux PC qui n'arrivent pas à se joindre). La webcam
   se choisit dans Mes réglages, onglet Voix ; changer de salon l'éteint.
8. **Mes réglages** (roue en bas à gauche, ou ton pseudo sur l'accueil) :
   pseudo, micro, sortie audio et webcam, sensibilité du micro avec un **test du micro
   hors d'un salon** (et « M'entendre » pour s'écouter), appui-pour-parler,
   **suppression du bruit** (ventilateur, clavier), **annulation d'écho** (pour
   jouer sur haut-parleurs sans casque), gain automatique, volume des autres,
   **sons d'interface** (quelqu'un entre ou sort du salon, micro ou son coupé,
   mention) et leur volume, raccourcis, dossier de données. Ces réglages
   restent sur ton PC et s'appliquent tout de suite, même en plein appel.
   Si tu débranches ton casque ou changes de sortie dans Windows en plein
   appel, le son suit tout seul ; un micro ou une sortie choisis mais
   débranchés sont remplacés par ceux du système jusqu'à leur retour.
   L'onglet Données montre la **version** de l'app et ouvre le **journal**
   (`journal.txt`) : en cas de souci ou de plantage, c'est le fichier à
   envoyer. Relancer l'app (ou ouvrir un code d'invitation) alors qu'elle est
   déjà ouverte ramène simplement la fenêtre existante.
9. **Raccourcis** (onglet de « Mes réglages ») : couper le micro, couper le son,
   appui-pour-parler, et des **groupes de chuchotement** : tant que tu tiens la
   touche d'un groupe, seuls ses membres t'entendent (« te chuchote » s'affiche
   chez eux) ; quand tu la relâches, tout le salon t'entend de nouveau. Les
   raccourcis marchent même quand l'appli n'est pas au premier plan (en jeu).
10. **Paramètres du serveur** (roue à côté du nom du serveur) : l'invitation
   pour tout le monde ; pour le créateur et les admins, le nom, les salons
   (ajouter, renommer, supprimer), l'historique (gardé ou non, combien de
   messages par salon) ; le créateur nomme les admins.
11. **Expulser ou bannir** : clic droit sur un membre dans la liste de droite
    (créateur et admins). Expulser le déconnecte, il peut revenir ; bannir
    l'empêche de revenir et d'héberger le serveur, jusqu'à ce qu'on le
    débannisse (onglet « Membres » des paramètres du serveur).

### Le serveur ne dépend pas du créateur

Chaque membre garde une copie du serveur (nom, salons, admins, et historique si
le serveur le garde). Dans « Mes serveurs », **n'importe quel membre peut
cliquer « Ouvrir »** : si un ami héberge déjà, l'app se connecte chez lui ;
sinon c'est son PC qui héberge, avec les mêmes salons, le même historique et
**le même code d'invitation**. Le code mène toujours au membre qui héberge en
ce moment, et n'importe quel membre peut le partager.

Personne ne peut pour autant prendre le contrôle en ré-hébergeant :

- l'identifiant du serveur est lié à la clé du créateur ;
- la liste des admins est signée par le créateur ;
- les réglages sont signés par le créateur ou un admin ;
- chaque message est signé par son auteur (un hôte ne peut ni en modifier ni en
  inventer) ;
- la liste des bannis est signée par le créateur ou un admin, et un banni ne
  peut pas ré-héberger ;
- pour entrer, il faut prouver qu'on connaît le code d'invitation, et l'hôte
  doit le prouver aussi.

Les copies se complètent : à chaque connexion, un membre envoie à l'hôte les
messages qu'il a gardés et que l'hôte n'a pas (écrits quand un autre hébergeait),
et l'hôte les fait suivre à tous. Le code d'invitation contient aussi le début
de l'empreinte du certificat de l'hôte : quelqu'un sur le réseau ne peut pas se
faire passer pour lui (voir les limites dans [Sécurité](#sécurité)).

### Quand l'hôte part ou lag

- En haut à gauche : **qui héberge** le serveur en ce moment, et le ping vers lui.
- **L'hôte se déconnecte ou plante** : le vocal continue (il passe de PC à PC,
  pas par l'hôte), et le membre suivant dans l'ordre de succession reprend le
  serveur en quelques secondes. Personne n'est déconnecté ni ne repasse par le
  code d'invitation ; un bandeau « Changement d'hôte… » s'affiche pendant ce
  temps. L'ordre est décidé à l'avance (le plus facile à joindre d'abord) et
  connu de tous, donc pas de vote ni d'attente dans ce cas.
- **L'hôte lag** : « Devenir l'hôte » sous le nom du serveur. Les autres ont
  15 secondes pour voter Oui ou Non ; la majorité de ceux qui répondent
  décide, et si c'est oui l'hôte passe la main sans couper personne.
- **Quitter un salon vocal** ne coupe que toi : les autres restent connectés
  entre eux.

Quand des copies divergent (un admin a changé un salon pendant que tu étais
absent), la plus récente l'emporte dès que vous vous reconnectez.

### Tes données

Tout est dans un seul dossier, affiché en bas de l'écran d'accueil
(`%APPDATA%\RIP Discord` sous Windows, `~/.local/share/RIP Discord` sous Linux) :

| Fichier | Contenu |
|---|---|
| `identity.key` | ta clé privée (ton identité) : ne la partage jamais |
| `servers/<id>/card.bin` | ta copie signée du serveur |
| `servers/<id>/history.bin` | les messages gardés, si le serveur garde l'historique |
| `servers/<id>/files/` | les images et fichiers envoyés ou vus dans ce serveur |

« Oublier » un serveur efface sa copie de ta machine. Si le créateur ou un
admin **désactive l'historique**, rien n'est plus écrit sur disque et les copies
existantes sont effacées chez chaque membre (tout de suite s'il est connecté,
sinon à sa prochaine connexion). Le dossier se change avec la variable
`RIP_DISCORD_DATA`. Rien ne passe par un serveur tiers.

## Faut-il ouvrir des ports ?

**Non, et il n'y a rien à installer à côté** : pas de serveur, pas de Raspberry,
ce sont vos PC qui hébergent. Voici comment l'app s'en sort seule :

1. **Annuaire décentralisé** : pour savoir quel membre héberge, l'app utilise
   le réseau DHT de BitTorrent, des millions de PC dans le monde qui gardent
   des petites valeurs. L'hôte y écrit son adresse, **chiffrée** avec une clé
   tirée du code d'invitation. Sans le code, impossible de savoir où regarder
   ni de lire. Aucun message ni la voix n'y passent.
2. **Perforation UDP** (« hole punching ») : l'ami qui arrive écrit lui aussi
   son adresse (chiffrée) dans l'annuaire ; l'hôte la voit et lui envoie un
   paquet pendant que l'ami se connecte. Chaque box croit répondre à une
   demande de l'intérieur et laisse passer. Pour connaître l'adresse publique
   de chaque PC, l'app pose la question à un serveur STUN public (Google,
   Cloudflare), qui ne voit qu'une requête vide.
3. **UPnP**, en bonus : si ta box l'accepte, l'app ouvre aussi le port, ce qui
   règle les cas difficiles.

La carte « Inviter des amis » affiche « Joignable sans ouvrir de port » quand
tout va bien. Le code d'invitation sert aussi de clé : partage-le entre amis
seulement.

Ce qui ne passe pas encore : les box ou opérateurs à NAT « symétrique » (rare
sur les box françaises, plus fréquent en 4G/5G et sur certains réseaux
d'entreprise ; l'app prévient quand elle le détecte). Pour eux, un **relais**
via un membre joignable arrive dans une prochaine version ; en attendant,
l'ouverture manuelle ci-dessous marche. Un réseau qui bloque tout l'UDP
(certains Wi-Fi publics ou d'entreprise) empêche aussi l'app de fonctionner
hors réseau local.

### Tuto : ouvrir le port à la main

Utile seulement si ta box est à NAT symétrique, ou si un ami n'arrive pas à
entrer. Il faut rediriger le port **7878 en UDP** de ta box vers ton PC.

1. **Trouve l'adresse locale de ton PC.** Windows : touche Windows, tape `cmd`,
   puis `ipconfig` ; note l'« Adresse IPv4 » (ex. `192.168.1.20`).
2. **Ouvre l'interface de ta box** dans un navigateur :
   Freebox `mafreebox.freebox.fr`, Livebox `192.168.1.1`,
   Bbox `mabbox.bytel.fr`, SFR `192.168.1.1` (identifiants sous la box ou dans
   l'appli de l'opérateur).
3. **Crée une redirection de port** (menus « NAT/PAT », « Redirection de
   ports » ou « Gestion des ports ») :
   - protocole : **UDP** ;
   - port externe : **7878** ; port interne : **7878** ;
   - appareil ou IP de destination : l'adresse de l'étape 1.
4. **Pare-feu Windows** : l'installeur autorise déjà l'app. Sinon, accepte la
   fenêtre « Autoriser l'accès » au premier lancement.
5. **Relance le serveur** : la carte « Inviter des amis » doit afficher
   « Accessible depuis Internet ». Partage le **code d'invitation** comme
   d'habitude : une adresse `IP:7878` seule ne suffit pas, l'hôte exige le code.

Astuce : dans la box, donne une IP fixe (« bail DHCP statique ») à ton PC, sinon
la redirection peut pointer vers une autre machine après un redémarrage.

Cas où même l'ouverture manuelle ne suffit pas : **CGNAT** de l'opérateur
(adresse publique partagée, fréquent en 4G/5G et chez certains FAI fibre ; on
peut souvent demander une IP dédiée au FAI) et **double box** en cascade
(ouvrir le port sur les deux, ou passer la première en mode bridge). L'app
prévient quand elle le détecte.

## Sécurité

Ce qui est protégé :

- tout passe en QUIC (TLS 1.3), chiffré entre chaque paire de PC ;
- **le code d'invitation est une clé** : pour entrer, il faut prouver qu'on le
  connaît, sans l'envoyer. Quelqu'un qui trouve l'adresse d'un hôte (en
  scannant Internet, par exemple) ne peut pas entrer sans le code. L'hôte doit
  le prouver lui aussi : une fausse adresse dans l'annuaire est démasquée ;
- messages, réglages, admins, bannis et code sont signés (Ed25519) : ni un
  hôte ni un membre ne peut les falsifier ;
- l'annuaire du DHT est chiffré avec une clé tirée du code ;
- les pièces jointes sont vérifiées par leur empreinte, jamais ouvertes
  automatiquement, et marquées « venues d'Internet » sous Windows (Windows
  avertit avant de lancer un programme reçu).

Limites à connaître :

- **Qui a le code est membre.** Un ancien membre qui a gardé le code peut
  revenir, même banni, en se créant un autre compte. Si le code a fuité ou
  après un bannissement, **change-le** : Paramètres du serveur > Invitation >
  « Nouveau code ». Les membres connectés reçoivent le nouveau tout seuls ;
  les autres devront te le redemander.
- **L'hôte du moment lit les messages** qu'il relaie (comme tout membre) : il
  ne peut ni les modifier ni en inventer, mais ce n'est pas un chiffrement de
  bout en bout entre auteur et lecteurs.
- **Ton adresse IP** est visible des autres membres (connexions directes),
  comme dans tout logiciel pair à pair.
- L'app n'a pas eu d'audit de sécurité externe.

Une faille ? Merci de la signaler en privé (voir [SECURITY.md](SECURITY.md)).

## Licence

Au choix, [MIT](LICENSE-MIT) ou [Apache-2.0](LICENSE-APACHE).

## Publier une version

```sh
# changer `version` dans Cargo.toml, puis :
git tag v0.1.0 && git push origin v0.1.0
```

Le workflow `Installeur Windows` compile, construit l'installeur NSIS
(`installer/rip-discord.nsi`) et le publie dans une Release. Sur chaque PR,
l'installeur est aussi construit et disponible en artefact du workflow.

## Arborescence

```
src/
├── audio/               son des salons vocaux : micro, sortie (cpal), Opus, mixage,
│   │                    écho et bruit retirés du micro, sons d'interface
│   └── share.rs         son du partage d'écran regardé (stéréo, son propre volume)
├── capture/             partage d'écran : écrans et fenêtres (xcap ; sous Windows
│   │                    Windows.Graphics.Capture), curseur, qualité
│   ├── video.rs         vidéo H.264 (OpenH264) : encodage, décodage sur un fil à part
│   └── sound.rs         son de la fenêtre ou de l'écran partagé (WASAPI par processus)
├── main.rs              sans argument : interface graphique ; `host`/`join` : terminal
├── lib.rs
├── core/                cœur réseau partagé
│   ├── protocol.rs      Packet, Control (Hello, Welcome, CardUpdate, Voice*…), Chat
│   ├── card.rs          serveur : salons, admins, historique, signatures
│   ├── identity.rs      clé Ed25519 de l'utilisateur
│   ├── store.rs         dossier de données : identité, serveurs, historique, fichiers
│   ├── journal.rs       journal de l'app (journal.txt), mis de côté au-delà de 1 Mo
│   ├── files.rs         envoi et téléchargement des pièces jointes
│   ├── limit.rs         anti-spam : nombre de messages par seconde
│   ├── framing.rs       lecture/écriture des paquets sur un flux QUIC
│   ├── invite.rs        codes d'invitation (adresse et/ou identifiant du serveur)
│   ├── directory.rs     annuaire chiffré sur le DHT : qui héberge, qui arrive
│   ├── nat.rs           adresse publique via STUN, sur le socket même de QUIC
│   └── tls.rs           certificat auto-signé, configs QUIC client/serveur
├── server/              mode hôte
│   ├── mod.rs           Host : accepte les pairs, relaie texte, vocal, réglages
│   ├── migration.rs     élection du successeur, hôte d'un salon vocal
│   ├── announce.rs      annonce de l'hôte dans l'annuaire, perforation vers les arrivants
│   └── upnp.rs          ouverture automatique du port sur la box
├── client/
│   ├── mod.rs           Client + Event, indépendants de l'UI
│   ├── join.rs          rejoindre : adresse directe, puis annuaire + perforation
│   ├── station.rs       le port QUIC du membre, gardé toute la session
│   ├── mesh.rs          vocal direct entre participants d'un salon
│   ├── failover.rs      relève : suivre l'ordre de succession quand l'hôte part
│   └── view.rs          état d'un serveur côté membre + copie locale
└── ui/
    ├── gui/             interface graphique (egui)
    │   ├── mod.rs       accueil, serveur (salons, vocal, membres)
    │   ├── chat.rs      fil de messages et saisie (gardée par salon) : réponses, réactions, fichiers
    │   ├── notify.rs    notifications Windows
    │   ├── me.rs        mes réglages : pseudo, micro, sortie, raccourcis, données
    │   ├── server.rs    paramètres du serveur : invitation, salons, historique, admins
    │   ├── net.rs       session réseau, son, relève, ouverture du port
    │   ├── keys.rs      raccourcis globaux (micro, son, appui-pour-parler, chuchoter)
    │   ├── single.rs    une seule fenêtre : un nouveau lancement ramène celle ouverte
    │   └── style.rs     couleurs, petits widgets, aide
    └── terminal.rs      interface terminal
tests/local_session.rs   texte, historique, vocal, écran, réglages signés, ré-hébergement
tests/messages.rs        réponses, réactions, suppressions, copie de l'historique,
                         expulsion, bannissement, anti-spam, fichiers
tests/releve.rs          relève d'hôte sans déconnexion, vote pour changer d'hôte
tests/vocal_direct.rs    voix, chuchotement, écran et son partagés de pair à pair
tests/annuaire.rs        annuaire sur un DHT local : trouver l'hôte, éviter les doublons
installer/               script NSIS de l'installeur Windows
```

## Commandes

```sh
cargo run                                       # interface graphique
cargo run -- host --name Alice                  # crée un serveur, l'héberge sur 0.0.0.0:7878 et y participe
cargo run -- servers                            # serveurs connus sur cette machine
cargo run -- host --name Max --server 9700      # ré-héberge un serveur connu (nom ou début d'id)
cargo run -- host --server 9700 --headless      # relais seul
cargo run -- join G00E-0R9Q-3VZG-… --name Max   # code ou IP:port
cargo test
```

Dans le terminal : toute ligne est envoyée dans le salon texte courant ;
`/salons`, `/salon <nom>`, `/vocal <nom>`, `/vocal` (quitter), `/ping`,
`/peers`, `/quit`. Logs : `RUST_LOG=rip_discord=debug`.

Pour tester sans Internet : `cargo run -- dev-network` lance un petit DHT et un
serveur STUN locaux et affiche les variables `RIP_DISCORD_DHT_BOOTSTRAP` et
`RIP_DISCORD_STUN` à donner aux autres instances.
Sans webcam, `RIP_DISCORD_FAKE_CAMERA=1` la remplace par une mire animée.

## Prochaines étapes

1. **Relais** pour les NAT symétriques via un participant joignable (aujourd'hui
   c'est l'hôte du serveur qui relaie la voix de ceux qui ne se joignent pas
   directement).
2. **Installeur signé** : demande un certificat de signature de code, sinon
   Windows SmartScreen continue d'avertir au premier lancement.
