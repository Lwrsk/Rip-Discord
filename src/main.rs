// Pas de console noire derrière la fenêtre sous Windows (builds release).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rip_discord::{
    client::{self, Client},
    core::{
        DEFAULT_PORT, VERSION,
        card::{Role, ServerCard},
        directory::Directory,
        invite::{self, Invite},
        journal, nat,
        store::Store,
    },
    server::{
        Host,
        announce::{Hosting, announce},
        upnp,
    },
    ui,
};

#[derive(Parser)]
#[command(version, about = "Chat local et privé à hôte dynamique")]
struct Cli {
    /// Sans sous-commande : ouvre l'interface graphique.
    #[command(subcommand)]
    mode: Option<Mode>,
    /// Interface graphique avec ce code d'invitation déjà rempli.
    #[arg(long)]
    code: Option<String>,
    /// Interface graphique avec ce pseudo.
    #[arg(long)]
    name: Option<String>,
}

#[derive(Subcommand)]
enum Mode {
    /// Héberger un serveur en terminal (et y participer, sauf --headless).
    Host {
        #[arg(long, default_value_t = SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)))]
        bind: SocketAddr,
        #[arg(long, default_value = "hôte")]
        name: String,
        /// Serveur déjà connu à ré-héberger (nom ou début d'identifiant, voir
        /// `servers`). Sans cette option, un nouveau serveur est créé.
        #[arg(long)]
        server: Option<String>,
        /// Relais seul, sans participant local.
        #[arg(long)]
        headless: bool,
    },
    /// Lister les serveurs connus sur cette machine (créés ou rejoints).
    Servers,
    /// Rejoindre une session existante en terminal.
    Join {
        /// Code d'invitation ou adresse de l'hôte, ex. 192.168.1.20:7878
        #[arg(default_value_t = format!("127.0.0.1:{DEFAULT_PORT}"))]
        code: String,
        #[arg(long, default_value = "invité")]
        name: String,
    },
    /// Développement : petit réseau DHT et serveur STUN locaux, pour tester
    /// sans Internet (voir `RIP_DISCORD_DHT_BOOTSTRAP` et `RIP_DISCORD_STUN`).
    #[command(hide = true)]
    DevNetwork {
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::Ipv4Addr,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    start_journal(cli.mode.is_none());
    tracing::info!(
        "RIP Discord {VERSION} ({} {})",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    match cli.mode {
        None => ui::gui::run(cli.code, cli.name),
        Some(mode) => tokio::runtime::Runtime::new()?.block_on(terminal(mode)),
    }
}

/// Messages dans la console et, pour l'interface graphique, dans le journal
/// du dossier de données ; un plantage y est noté (et affiché sous Windows,
/// où l'app n'a pas de console).
fn start_journal(gui: bool) {
    use tracing_subscriber::{filter::Targets, fmt, prelude::*};

    // `RUST_LOG=rip_discord=debug` comme d'habitude (cibles et niveaux, sans
    // les expressions d'`EnvFilter`, qui tirent tout un moteur de regex).
    let filter: Targets = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| "rip_discord=info".parse().unwrap());
    let file = gui
        .then(|| Store::default_root().ok())
        .flatten()
        .and_then(|root| journal::open(&root).ok());
    let to_file = file.map(|f| fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f)));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(to_file)
        .init();

    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("?");
        tracing::error!("plantage (fil {thread}) : {info}");
        default(info);
        #[cfg(windows)]
        if gui && thread == "main" {
            let journal = Store::default_root()
                .map(|r| journal::path(&r).display().to_string())
                .unwrap_or_default();
            rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("RIP Discord s'est arrêté")
                .set_description(format!(
                    "Désolé, l'app a planté.\n\nCe qui s'est passé est noté dans le journal :\n{journal}\n\nEnvoie ce fichier \
                     à celui qui s'occupe de l'app pour qu'il puisse corriger."
                ))
                .show();
        }
    }));
}

async fn terminal(mode: Mode) -> Result<()> {
    let store = Store::open_default()?;
    let me = store.identity()?;
    match mode {
        Mode::Servers => {
            println!("Données : {}", store.root().display());
            for card in store.servers() {
                let role = match card.role(&me.id()) {
                    Role::Owner => "créateur",
                    Role::Admin => "admin",
                    Role::Member => "membre",
                };
                println!("  {}  {}  ({role})", &card.id.to_hex()[..8], card.name());
            }
        }
        Mode::DevNetwork { bind } => {
            let testnet = mainline::Testnet::builder(8).bind_address(bind).build()?;
            let stun = tokio::net::UdpSocket::bind((bind, 3478)).await?;
            println!("RIP_DISCORD_DHT_BOOTSTRAP={}", testnet.bootstrap.join(","));
            println!("RIP_DISCORD_STUN={}", stun.local_addr()?);
            nat::run_stun_server(stun).await;
        }
        Mode::Host {
            bind,
            name,
            server,
            headless,
        } => {
            let card = match server {
                Some(wanted) => store
                    .servers()
                    .into_iter()
                    .find(|c| c.id.to_hex().starts_with(&wanted.to_lowercase()) || c.name() == wanted)
                    .with_context(|| format!("serveur « {wanted} » inconnu sur cette machine"))?,
                None => store.save_card(&ServerCard::create(&me, &format!("Serveur de {name}")))?,
            };
            println!("Héberge « {} » ({})", card.name(), &card.id.to_hex()[..8]);
            // Avec un participant local, c'est lui qui écrit l'historique sur disque.
            let history = match headless {
                true => store.history(&card),
                false => store.history(&card).detached(),
            };
            let host = Host::bind(bind, card.clone(), history)?.with_store(store.clone());
            let port = host.local_addr()?.port();
            let lan = upnp::lan_ip().map(|ip| SocketAddr::new(ip, port));
            let key = card.access_key();
            let mut code = Invite::host(lan.unwrap_or(host.local_addr()?), &host.fingerprint(), key);
            let mut _announcement = None;
            match Directory::start().await {
                Ok(dir) => match announce(&dir, &host, me.id()).await {
                    Ok(Hosting::Announced(a)) => {
                        _announcement = Some(a);
                        code = Invite::key(key);
                    }
                    Ok(Hosting::AlreadyHosted(other)) => bail!(
                        "ce serveur est déjà hébergé par un autre membre ({}) : rejoins-le avec `join {}`",
                        other.addr.public,
                        invite::encode(&Invite::key(key))
                    ),
                    Err(e) => eprintln!("Annuaire indisponible ({e:#}) : code valable en réseau local seulement."),
                },
                Err(e) => eprintln!("Annuaire indisponible ({e:#}) : code valable en réseau local seulement."),
            }
            println!("Code d'invitation : {}", invite::encode(&code));
            let relay = tokio::spawn(host.run());
            if headless {
                relay.await??;
            } else {
                // L'hôte est aussi un participant : il se connecte à son propre relais.
                let local = SocketAddr::from(([127, 0, 0, 1], port));
                let (client, events) = Client::connect(local, key, &me, &name).await?;
                ui::terminal::run(client, events, &name, Some(store)).await?;
            }
        }
        Mode::Join { code, name } => {
            let code = invite::decode(&code)?;
            let (client, events) = client::join(&code, &me, &name).await?;
            ui::terminal::run(client, events, &name, Some(store)).await?;
        }
    }
    Ok(())
}
