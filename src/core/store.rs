//! Données gardées sur la machine, toutes dans un seul dossier que
//! l'utilisateur peut sauvegarder, copier ou effacer :
//!
//! ```text
//! <données>/RIP Discord/
//! ├── identity.key              clé privée de l'utilisateur (ne jamais partager)
//! └── servers/<id>/
//!     ├── card.bin              copie signée du serveur (nom, salons, admins)
//!     ├── history.bin           messages gardés, si le serveur garde l'historique
//!     └── files/<sha256>        pièces jointes reçues ou envoyées
//! ```
//!
//! Le dossier se change avec la variable d'environnement `RIP_DISCORD_DATA`.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use tracing::warn;

use super::{
    card::{ChannelId, HistoryPolicy, Kind, MessageId, ServerCard, ServerId, SignedMessage},
    identity::{Identity, UserId, hex},
};

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Dossier par défaut (`%APPDATA%\RIP Discord` sous Windows,
    /// `~/.local/share/RIP Discord` sous Linux…).
    pub fn open_default() -> Result<Self> {
        Self::at(Self::default_root()?)
    }

    /// Dossier de données de l'utilisateur (ou `RIP_DISCORD_DATA`).
    pub fn default_root() -> Result<PathBuf> {
        Ok(match std::env::var_os("RIP_DISCORD_DATA") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_dir()
                .context("dossier de données introuvable")?
                .join("RIP Discord"),
        })
    }

    pub fn at(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("servers")).with_context(|| format!("création de {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Identité de l'utilisateur, créée au premier appel.
    pub fn identity(&self) -> Result<Identity> {
        let path = self.root.join("identity.key");
        if let Ok(bytes) = fs::read(&path) {
            return Identity::from_pkcs8(&bytes);
        }
        let me = Identity::generate();
        write_atomic(&path, me.to_pkcs8()).with_context(|| format!("écriture de {}", path.display()))?;
        Ok(me)
    }

    /// Copie de sauvegarde de ma clé (pour la retrouver après une
    /// réinstallation ou sur un autre PC).
    pub fn export_identity(&self, to: &Path) -> Result<()> {
        let me = self.identity()?;
        write_atomic(to, me.to_pkcs8()).with_context(|| format!("écriture de {}", to.display()))
    }

    /// Reprend une clé sauvegardée. L'ancienne est gardée à côté, au cas où.
    /// Prise en compte au prochain lancement de l'app.
    pub fn import_identity(&self, from: &Path) -> Result<Identity> {
        let bytes = fs::read(from).with_context(|| format!("lecture de {}", from.display()))?;
        let imported = Identity::from_pkcs8(&bytes).context("ce fichier n'est pas une clé RIP Discord")?;
        let path = self.root.join("identity.key");
        if let Ok(old) = fs::read(&path)
            && old != bytes
        {
            write_atomic(&self.root.join("identity-avant-import.key"), &old)?;
        }
        write_atomic(&path, &bytes)?;
        Ok(imported)
    }

    fn server_dir(&self, id: ServerId) -> PathBuf {
        self.root.join("servers").join(id.to_hex())
    }

    /// Serveurs connus (créés ou rejoints), triés par nom.
    pub fn servers(&self) -> Vec<ServerCard> {
        let mut cards: Vec<ServerCard> = fs::read_dir(self.root.join("servers"))
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| fs::read(e.path().join("card.bin")).ok())
            .filter_map(|b| ServerCard::decode(&b))
            .filter(|c| c.verify().is_ok())
            .collect();
        cards.sort_by_key(|c| c.name().to_lowercase());
        cards
    }

    pub fn card(&self, id: ServerId) -> Option<ServerCard> {
        let bytes = fs::read(self.server_dir(id).join("card.bin")).ok()?;
        ServerCard::decode(&bytes).filter(|c| c.verify().is_ok())
    }

    /// Enregistre la carte, fusionnée avec la copie déjà présente.
    /// Renvoie la version gardée.
    pub fn save_card(&self, card: &ServerCard) -> Result<ServerCard> {
        let mut kept = card.clone();
        if let Some(mut old) = self.card(card.id) {
            old.merge(card)?;
            kept = old;
        }
        let dir = self.server_dir(card.id);
        fs::create_dir_all(&dir)?;
        write_atomic(&dir.join("card.bin"), &postcard::to_stdvec(&kept)?)?;
        Ok(kept)
    }

    pub fn history(&self, card: &ServerCard) -> HistoryLog {
        let mut log = HistoryLog::open(self.server_dir(card.id).join("history.bin"), card.id, card.history());
        log.set_moderators(card);
        log
    }

    /// Où est (ou sera) rangée une pièce jointe.
    pub fn file_path(&self, server: ServerId, hash: &[u8; 32]) -> PathBuf {
        self.server_dir(server).join("files").join(hex(hash))
    }

    /// Contenu d'une pièce jointe déjà reçue.
    pub fn file(&self, server: ServerId, hash: &[u8; 32]) -> Option<Vec<u8>> {
        fs::read(self.file_path(server, hash)).ok()
    }

    /// Range une pièce jointe (dont l'empreinte a été vérifiée).
    pub fn save_file(&self, server: ServerId, hash: &[u8; 32], bytes: &[u8]) -> Result<PathBuf> {
        let path = self.file_path(server, hash);
        if !path.exists() {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            write_atomic(&path, bytes)?;
        }
        Ok(path)
    }

    /// Efface tout ce que cette machine sait du serveur.
    pub fn forget(&self, id: ServerId) -> Result<()> {
        let dir = self.server_dir(id);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    // Fichier temporaire propre à chaque écriture : l'hôte et l'interface
    // peuvent enregistrer la même carte en même temps.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("{}.{n}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Historique d'un serveur : en mémoire, et sur disque si `path` est défini
/// et que la politique du serveur le permet.
pub struct HistoryLog {
    path: Option<PathBuf>,
    server: ServerId,
    policy: HistoryPolicy,
    messages: Vec<SignedMessage>,
    seen: HashSet<Vec<u8>>,
    /// Créateur et admins : ils peuvent supprimer les messages des autres.
    moderators: Vec<UserId>,
    /// Messages supprimés → qui les a supprimés (la suppression peut arriver
    /// avant le message lui-même).
    deleted: HashMap<MessageId, Vec<UserId>>,
    /// Messages ajoutés depuis le dernier compactage.
    pushed: usize,
}

/// Un vrai message, et non une action sur un autre (modification, réaction,
/// suppression) : seuls eux comptent dans la limite de messages par salon.
fn is_content(m: &SignedMessage) -> bool {
    m.body.kind.target().is_none()
}

impl HistoryLog {
    /// Historique en mémoire seulement (tests, hôte sans disque).
    pub fn in_memory(server: ServerId, policy: HistoryPolicy) -> Self {
        Self {
            path: None,
            server,
            policy,
            messages: Vec::new(),
            seen: HashSet::new(),
            moderators: Vec::new(),
            deleted: HashMap::new(),
            pushed: 0,
        }
    }

    fn open(path: PathBuf, server: ServerId, policy: HistoryPolicy) -> Self {
        let mut log = Self::in_memory(server, policy);
        log.path = Some(path);
        if policy.keep
            && let Ok(bytes) = fs::read(log.path.as_ref().unwrap())
        {
            for msg in read_records(&bytes, server) {
                if log.seen.insert(msg.sig.clone()) {
                    log.note_delete(&msg);
                    log.messages.push(msg);
                }
            }
        }
        log.messages.sort_by_cached_key(|m| m.order());
        log.set_policy(policy);
        log
    }

    /// Copie en mémoire, sans lien avec le disque. Pour l'hôte quand un client
    /// local tient déjà le fichier (un seul écrivain par fichier).
    pub fn detached(&self) -> Self {
        Self {
            path: None,
            server: self.server,
            policy: self.policy,
            messages: self.messages.clone(),
            seen: self.seen.clone(),
            moderators: self.moderators.clone(),
            deleted: self.deleted.clone(),
            pushed: 0,
        }
    }

    pub fn messages(&self) -> &[SignedMessage] {
        &self.messages
    }

    pub fn policy(&self) -> HistoryPolicy {
        self.policy
    }

    /// Qui peut supprimer les messages des autres (créateur et admins).
    pub fn set_moderators(&mut self, card: &ServerCard) {
        self.moderators = std::iter::once(card.owner)
            .chain(card.admins.body.admins.iter().copied())
            .collect();
    }

    /// Applique une nouvelle version du serveur : politique, admins, salons.
    pub fn apply_card(&mut self, card: &ServerCard) {
        self.set_moderators(card);
        let settings = card.settings();
        self.set_policy(settings.history);
        self.retain_channels(|c| settings.is_text_channel(c));
    }

    /// `by` a-t-il le droit de supprimer ce message ?
    fn may_delete(&self, by: &UserId, msg: &SignedMessage) -> bool {
        *by == msg.signer || self.moderators.contains(by)
    }

    /// Retient une demande de suppression, même si le message n'est pas (encore) là.
    fn note_delete(&mut self, msg: &SignedMessage) {
        if let Kind::Delete { target } = msg.body.kind {
            let by = self.deleted.entry(target).or_default();
            if !by.contains(&msg.signer) {
                by.push(msg.signer);
            }
        }
    }

    /// Supprimé par quelqu'un qui en avait le droit.
    fn is_deleted(&self, msg: &SignedMessage) -> bool {
        self.deleted
            .get(&msg.id())
            .is_some_and(|by| by.iter().any(|by| self.may_delete(by, msg)))
    }

    /// Ajoute un message (déjà vérifié). Renvoie `false` s'il était déjà connu,
    /// ou trop ancien pour être gardé.
    ///
    /// Les copies se fusionnent ainsi sans doublon (un message n'existe qu'une
    /// fois, reconnu à sa signature) et dans le même ordre partout : chaque
    /// message prend sa place selon [`SignedMessage::order`], même s'il arrive
    /// après des plus récents (repris de la copie d'un membre).
    ///
    /// Une suppression retire aussi du disque le message visé, ses
    /// modifications et ses réactions : il n'en reste que la trace « supprimé ».
    pub fn push(&mut self, msg: SignedMessage) -> bool {
        if msg.from_future() || !self.seen.insert(msg.sig.clone()) {
            return false;
        }
        if !self.policy.keep {
            return true;
        }
        if self.beyond_limit(&msg) {
            // Le salon a déjà assez de messages plus récents : la limite le
            // couperait aussitôt, inutile de l'écrire ni de le rediffuser.
            return false;
        }
        if let Kind::Delete { target } = msg.body.kind {
            self.note_delete(&msg);
            let before = self.messages.len();
            let allowed = self
                .messages
                .iter()
                .find(|m| m.id() == target)
                .is_some_and(|m| self.may_delete(&msg.signer, m));
            if allowed {
                self.messages.retain(|m| {
                    m.id() != target
                        && (m.body.kind.target() != Some(target) || matches!(m.body.kind, Kind::Delete { .. }))
                });
            }
            if self.messages.len() != before {
                self.insert(msg);
                self.rewrite();
                return true;
            }
        } else if self.is_deleted(&msg) {
            // Déjà supprimé ailleurs : on ne le garde pas.
            return true;
        }
        if let Some(path) = &self.path
            && let Err(e) = append_record(path, &msg)
        {
            warn!("historique : écriture impossible : {e:#}");
        }
        self.insert(msg);
        let max = self.policy.max_per_channel as usize;
        self.pushed += 1;
        if max > 0 && self.pushed >= max {
            // Compactage de temps en temps, pas à chaque message.
            self.set_policy(self.policy);
        }
        true
    }

    /// Range un message à sa place dans le fil (le plus souvent à la fin).
    fn insert(&mut self, msg: SignedMessage) {
        let key = msg.order();
        let at = self.messages.partition_point(|m| m.order() <= key);
        self.messages.insert(at, msg);
    }

    /// Le salon de `msg` a-t-il déjà, sans lui, sa limite de messages plus récents ?
    fn beyond_limit(&self, msg: &SignedMessage) -> bool {
        let max = self.policy.max_per_channel as usize;
        if max == 0 {
            return false;
        }
        // Une action sur un message gardé est gardée avec lui.
        if let Some(target) = msg.body.kind.target()
            && self.messages.iter().any(|m| m.id() == target)
        {
            return false;
        }
        let key = msg.order();
        let at = self.messages.partition_point(|m| m.order() <= key);
        self.messages[at..]
            .iter()
            .filter(|m| m.body.channel == msg.body.channel && is_content(m))
            .take(max)
            .count()
            >= max
    }

    /// Applique une nouvelle politique : coupe ou efface ce qui dépasse.
    pub fn set_policy(&mut self, policy: HistoryPolicy) {
        self.policy = policy;
        if !policy.keep {
            self.messages.clear();
            if let Some(path) = &self.path {
                let _ = fs::remove_file(path);
                if let Some(dir) = path.parent() {
                    let _ = fs::remove_dir_all(dir.join("files"));
                }
            }
            return;
        }
        let before = self.messages.len();
        let max = policy.max_per_channel as usize;
        self.pushed = 0;
        if max > 0 {
            // Les `max` derniers messages de chaque salon, et l'heure du plus
            // ancien gardé quand le salon est plein.
            let mut per_channel: HashMap<ChannelId, usize> = HashMap::new();
            let mut cutoff: HashMap<ChannelId, (u64, MessageId)> = HashMap::new();
            let mut kept = HashSet::new();
            let present: HashSet<MessageId> = self.messages.iter().filter(|m| is_content(m)).map(|m| m.id()).collect();
            for m in self.messages.iter().rev().filter(|m| is_content(m)) {
                let n = per_channel.entry(m.body.channel).or_default();
                *n += 1;
                if *n <= max {
                    kept.insert(m.id());
                    if *n == max {
                        cutoff.insert(m.body.channel, m.order());
                    }
                }
            }
            // Une action suit son message ; sans lui (déjà supprimé, ou pas
            // encore arrivé), elle reste tant qu'elle est dans la limite.
            self.messages.retain(|m| match m.body.kind.target() {
                None => kept.contains(&m.id()),
                Some(target) => {
                    kept.contains(&target)
                        || (!present.contains(&target) && cutoff.get(&m.body.channel).is_none_or(|c| m.order() >= *c))
                }
            });
        }
        if self.messages.len() != before {
            self.rewrite();
        }
    }

    /// Retire les messages des salons qui n'existent plus.
    pub fn retain_channels(&mut self, exists: impl Fn(ChannelId) -> bool) {
        let before = self.messages.len();
        self.messages.retain(|m| exists(m.body.channel));
        if self.messages.len() != before {
            self.rewrite();
        }
    }

    fn rewrite(&self) {
        let Some(path) = &self.path else { return };
        let mut bytes = Vec::new();
        for m in &self.messages {
            encode_record(&mut bytes, m);
        }
        if let Err(e) = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .map_err(anyhow::Error::from)
            .and_then(|_| write_atomic(path, &bytes))
        {
            warn!("historique : réécriture impossible : {e:#}");
        }
        self.prune_files();
    }

    /// Efface les pièces jointes dont plus aucun message ne parle. Celles
    /// arrivées depuis peu sont épargnées : le message qui les cite peut être
    /// encore en route.
    fn prune_files(&self) {
        let Some(dir) = self.path.as_ref().and_then(|p| p.parent()).map(|d| d.join("files")) else {
            return;
        };
        let Ok(entries) = fs::read_dir(&dir) else { return };
        let used: HashSet<String> = self
            .messages
            .iter()
            .filter_map(|m| match &m.body.kind {
                Kind::Rich { files, .. } => Some(files.iter().map(|f| hex(&f.hash))),
                _ => None,
            })
            .flatten()
            .collect();
        let recent = SystemTime::now() - Duration::from_secs(600);
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let old = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < recent);
            if old && !used.contains(&name) {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    pub fn server(&self) -> ServerId {
        self.server
    }
}

fn encode_record(out: &mut Vec<u8>, msg: &SignedMessage) {
    let bytes = postcard::to_stdvec(msg).expect("sérialisation en mémoire");
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&bytes);
}

fn append_record(path: &Path, msg: &SignedMessage) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut bytes = Vec::new();
    encode_record(&mut bytes, msg);
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&bytes)?;
    Ok(())
}

/// Lit les enregistrements ; s'arrête proprement sur une fin tronquée
/// (coupure pendant une écriture).
fn read_records(mut bytes: &[u8], server: ServerId) -> Vec<SignedMessage> {
    let mut out = Vec::new();
    while bytes.len() >= 4 {
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let Some(record) = bytes.get(4..4 + len) else { break };
        if let Some(m) = SignedMessage::decode(record, server) {
            out.push(m);
        }
        bytes = &bytes[4 + len..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::card::{Message, now_ms};

    fn temp_store() -> Store {
        // Un dossier par test : lancés en parallèle dans la même milliseconde,
        // deux tests s'effaçaient leurs fichiers.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("rip-discord-test-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Store::at(dir).unwrap()
    }

    fn msg(me: &Identity, card: &ServerCard, channel: ChannelId, body: &str) -> SignedMessage {
        Message {
            server: card.id,
            channel,
            author_name: "moi".into(),
            // Dans l'ordre d'écriture, comme les envoie un client.
            at_ms: {
                static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                now_ms() + N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            body: body.into(),
            kind: Kind::Text,
        }
        .sign(me)
    }

    #[test]
    fn identite_carte_et_historique_persistants() {
        let store = temp_store();
        let me = store.identity().unwrap();
        assert_eq!(store.identity().unwrap().id(), me.id());

        // Sauvegarde puis reprise sur un autre PC : même identité.
        let backup = store.root().join("sauvegarde.key");
        store.export_identity(&backup).unwrap();
        let other = Store::at(store.root().join("autre-pc")).unwrap();
        let fresh = other.identity().unwrap();
        assert_eq!(other.import_identity(&backup).unwrap().id(), me.id());
        assert_eq!(other.identity().unwrap().id(), me.id());
        // L'ancienne clé du PC est gardée à côté.
        let kept = fs::read(other.root().join("identity-avant-import.key")).unwrap();
        assert_eq!(Identity::from_pkcs8(&kept).unwrap().id(), fresh.id());
        // Un fichier quelconque est refusé, sans rien toucher.
        fs::write(&backup, b"pas une cle").unwrap();
        assert!(other.import_identity(&backup).is_err());
        assert_eq!(other.identity().unwrap().id(), me.id());

        let card = ServerCard::create(&me, "Les potes");
        store.save_card(&card).unwrap();
        assert_eq!(store.servers(), vec![card.clone()]);

        let mut log = store.history(&card);
        let m = msg(&me, &card, 1, "un");
        assert!(log.push(m.clone()));
        assert!(!log.push(m));
        log.push(msg(&me, &card, 1, "deux"));
        assert_eq!(store.history(&card).messages().len(), 2);

        // Limite par salon.
        let mut log = store.history(&card);
        log.set_policy(HistoryPolicy {
            keep: true,
            max_per_channel: 1,
        });
        assert_eq!(log.messages()[0].body.body, "deux");
        assert_eq!(store.history(&card).messages().len(), 1);

        // Historique désactivé : tout est effacé du disque.
        log.set_policy(HistoryPolicy {
            keep: false,
            max_per_channel: 0,
        });
        log.push(msg(&me, &card, 1, "trois"));
        assert!(log.messages().is_empty());
        assert!(store.history(&card).messages().is_empty());

        store.forget(card.id).unwrap();
        assert!(store.servers().is_empty());
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn deux_copies_fusionnees_sans_doublon_dans_le_meme_ordre() {
        let me = Identity::generate();
        let friend = Identity::generate();
        let card = ServerCard::create(&me, "s");
        let at = |who: &Identity, at_ms: u64, body: &str| {
            Message {
                server: card.id,
                channel: 1,
                author_name: "x".into(),
                at_ms,
                body: body.into(),
                kind: Kind::Text,
            }
            .sign(who)
        };
        // Hier ensemble, puis chacun de son côté.
        let common = [at(&me, 10, "salut"), at(&friend, 20, "yo")];
        let mine: Vec<_> = common.iter().cloned().chain([at(&me, 300, "aujourd'hui")]).collect();
        let theirs: Vec<_> = common
            .iter()
            .cloned()
            .chain([at(&friend, 100, "hier soir"), at(&friend, 300, "même milliseconde")])
            .collect();

        let policy = HistoryPolicy {
            keep: true,
            max_per_channel: 0,
        };
        let merged = |first: &[SignedMessage], second: &[SignedMessage]| {
            let mut log = HistoryLog::in_memory(card.id, policy);
            for m in first.iter().chain(second) {
                log.push(m.clone());
            }
            log.messages().to_vec()
        };
        let a = merged(&mine, &theirs);
        let b = merged(&theirs, &mine);
        assert_eq!(a.len(), 5, "rien de perdu, rien en double");
        assert_eq!(a, b, "même ordre quel que soit le sens de la fusion");
        assert_eq!(a[2].body.body, "hier soir");

        // Salon plein : un vieux message repris d'une copie ne chasse pas les récents.
        let mut log = HistoryLog::in_memory(
            card.id,
            HistoryPolicy {
                keep: true,
                max_per_channel: 2,
            },
        );
        log.push(at(&me, 200, "deux"));
        log.push(at(&me, 300, "trois"));
        assert!(!log.push(at(&friend, 100, "trop vieux")));
        assert!(log.push(at(&friend, 250, "entre les deux")));
        log.set_policy(log.policy());
        let kept: Vec<_> = log.messages().iter().map(|m| m.body.body.as_str()).collect();
        assert_eq!(kept, ["entre les deux", "trois"]);
    }

    #[test]
    fn fin_de_fichier_tronquee() {
        let me = Identity::generate();
        let card = ServerCard::create(&me, "s");
        let mut bytes = Vec::new();
        encode_record(&mut bytes, &msg(&me, &card, 1, "ok"));
        encode_record(&mut bytes, &msg(&me, &card, 1, "coupé"));
        bytes.truncate(bytes.len() - 3);
        assert_eq!(read_records(&bytes, card.id).len(), 1);
    }

    #[test]
    fn suppression_efface_le_message_et_ses_traces() {
        let store = temp_store();
        let me = store.identity().unwrap();
        let other = Identity::generate();
        let card = ServerCard::create(&me, "s");
        store.save_card(&card).unwrap();
        let act = |who: &Identity, kind: Kind| {
            Message {
                server: card.id,
                channel: 1,
                author_name: "x".into(),
                at_ms: now_ms(),
                body: if matches!(kind, Kind::Edit { .. }) {
                    "modifié"
                } else {
                    ""
                }
                .into(),
                kind,
            }
            .sign(who)
        };
        let mut log = store.history(&card);
        let theirs = msg(&other, &card, 1, "secret");
        let target = theirs.id();
        log.push(theirs.clone());
        log.push(act(&other, Kind::Edit { target }));
        log.push(act(
            &me,
            Kind::React {
                target,
                emoji: "👍".into(),
                on: true,
            },
        ));
        assert_eq!(log.messages().len(), 3);

        // Un inconnu ne peut pas supprimer le message d'un autre…
        let stranger = Identity::generate();
        log.push(act(&stranger, Kind::Delete { target }));
        assert!(log.messages().iter().any(|m| m.id() == target));

        // … le créateur, si : il ne reste que la trace de la suppression.
        log.push(act(&me, Kind::Delete { target }));
        let left = store.history(&card);
        assert!(
            left.messages()
                .iter()
                .all(|m| m.body.body != "secret" && m.body.body != "modifié")
        );
        assert!(
            left.messages()
                .iter()
                .any(|m| m.signer == me.id() && matches!(m.body.kind, Kind::Delete { .. }))
        );

        // Le message supprimé, revenu d'une vieille copie, n'est pas repris.
        let mut log = store.history(&card);
        log.push(act(&other, Kind::Delete { target }));
        assert!(!log.push(theirs) || log.messages().iter().all(|m| m.id() != target));
        assert!(store.history(&card).messages().iter().all(|m| m.id() != target));
        let _ = fs::remove_dir_all(store.root());
    }

    #[test]
    fn pieces_jointes_rangees_par_empreinte() {
        let store = temp_store();
        let me = store.identity().unwrap();
        let card = ServerCard::create(&me, "s");
        let bytes = b"contenu".to_vec();
        let hash = crate::core::files::hash(&bytes);
        assert!(store.file(card.id, &hash).is_none());
        store.save_file(card.id, &hash, &bytes).unwrap();
        assert_eq!(store.file(card.id, &hash), Some(bytes));
        let _ = fs::remove_dir_all(store.root());
    }

    fn react(me: &Identity, card: &ServerCard, channel: ChannelId, target: MessageId, at_ms: u64) -> SignedMessage {
        Message {
            server: card.id,
            channel,
            author_name: "moi".into(),
            at_ms,
            body: String::new(),
            kind: Kind::React {
                target,
                emoji: "👍".into(),
                on: at_ms.is_multiple_of(2),
            },
        }
        .sign(me)
    }

    #[test]
    fn reactions_hors_limite_de_messages() {
        let me = Identity::generate();
        let card = ServerCard::create(&me, "s");
        let policy = HistoryPolicy {
            keep: true,
            max_per_channel: 10,
        };
        let mut log = HistoryLog::in_memory(card.id, policy);
        let first = msg(&me, &card, 1, "important");
        let target = first.id();
        log.push(first);
        for i in 0..50 {
            log.push(react(&me, &card, 1, target, now_ms() + 1 + i));
        }
        for i in 0..9 {
            log.push(msg(&me, &card, 1, &format!("suite {i}")));
        }
        log.set_policy(policy);
        let texts = log.messages().iter().filter(|m| is_content(m)).count();
        assert_eq!(texts, 10);
        assert!(log.messages().iter().any(|m| m.id() == target));
        assert_eq!(log.messages().len(), 60);

        // Un message de plus : le premier sort, ses réactions avec lui.
        log.push(msg(&me, &card, 1, "dernier"));
        log.set_policy(policy);
        assert!(log.messages().iter().all(|m| m.id() != target && is_content(m)));
        assert_eq!(log.messages().len(), 10);
    }

    #[test]
    fn message_du_futur_refuse() {
        let me = Identity::generate();
        let card = ServerCard::create(&me, "s");
        let mut log = HistoryLog::in_memory(card.id, HistoryPolicy::default());
        let futur = Message {
            server: card.id,
            channel: 1,
            author_name: "moi".into(),
            at_ms: now_ms() + 3_600_000,
            body: "dans une heure".into(),
            kind: Kind::Text,
        }
        .sign(&me);
        assert!(futur.from_future());
        assert!(!log.push(futur));
        assert!(log.push(msg(&me, &card, 1, "maintenant")));
        assert_eq!(log.messages().len(), 1);
    }
}
