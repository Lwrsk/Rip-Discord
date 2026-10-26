//! UI terminal : chaque ligne tapée est envoyée dans le salon texte courant.
//! Commandes : `/salons`, `/salon <nom>`, `/vocal <nom>`, `/vocal`, `/ping`,
//! `/peers`, `/quit`.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::client::{Client, Event, ServerView, TooFast};
use crate::core::{
    card::{ChannelId, Kind, SignedMessage},
    store::Store,
};

pub async fn run(
    mut client: Client,
    mut events: mpsc::UnboundedReceiver<Event>,
    my_name: &str,
    store: Option<Store>,
) -> Result<()> {
    let mut view = ServerView::new(&client, my_name, store);
    let mut channel: ChannelId = view.card.settings().text_channels.first().map_or(0, |c| c.id);
    println!(
        "Connecté à « {} » (id {}). {} autre(s) personne(s). Tape /aide pour les commandes.",
        view.card.name(),
        client.peer_id,
        client.initial_peers.len()
    );
    print_channels(&view, channel);

    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = stdin.next_line() => match line? {
                None => {
                    // Entrée fermée (pipe) : on laisse passer les réponses en cours puis on part.
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    while let Ok(ev) = events.try_recv() { print_event(&ev, &mut view, channel); }
                    break;
                }
                Some(line) => {
                    let line = line.trim();
                    let (cmd, arg) = line.split_once(' ').map_or((line, ""), |(c, a)| (c, a.trim()));
                    match cmd {
                        "" => {}
                        "/quit" => break,
                        "/aide" => println!("/salons  /salon <nom>  /vocal <nom>  /vocal (quitter)  /ping  /peers  /quit"),
                        "/ping" => client.ping()?,
                        "/peers" => {
                            for p in view.peers.values() { println!("  {} {} ({})", p.id, p.name, p.user.short()); }
                        }
                        "/salons" => print_channels(&view, channel),
                        "/salon" => match find(&view.card.settings().text_channels, arg) {
                            Some(id) => {
                                channel = id;
                                for m in view.messages(channel) { println!("{}", format_line(&view, m)); }
                            }
                            None => println!("salon texte « {arg} » introuvable"),
                        },
                        "/vocal" if arg.is_empty() => client.leave_voice()?,
                        "/vocal" => match find(&view.card.settings().voice_channels, arg) {
                            Some(id) => client.join_voice(id)?,
                            None => println!("salon vocal « {arg} » introuvable"),
                        },
                        _ => match client.send_text(channel, line) {
                            Ok(msg) => view.push_own(msg),
                            Err(e) if e.is::<TooFast>() => println!("* {e}"),
                            Err(e) => return Err(e),
                        },
                    }
                }
            },
            ev = events.recv() => match ev {
                Some(Event::Disconnected { reason }) => {
                    println!("Déconnecté : {reason}");
                    return Ok(());
                }
                None => return Ok(()),
                Some(ev) => print_event(&ev, &mut view, channel),
            },
        }
    }
    client.quit().await;
    Ok(())
}

fn find(channels: &[crate::core::card::Channel], name: &str) -> Option<ChannelId> {
    let name = name.trim_start_matches('#').to_lowercase();
    channels.iter().find(|c| c.name.to_lowercase() == name).map(|c| c.id)
}

fn print_channels(view: &ServerView, current: ChannelId) {
    let s = view.card.settings();
    let texts: Vec<_> = s
        .text_channels
        .iter()
        .map(|c| {
            if c.id == current {
                format!("[#{}]", c.name)
            } else {
                format!("#{}", c.name)
            }
        })
        .collect();
    println!("Texte : {}", texts.join("  "));
    for c in &s.voice_channels {
        let members = view.voice.get(&c.id).map(|r| {
            r.members
                .iter()
                .map(|m| {
                    if r.host == Some(*m) {
                        format!("{}*", view.peer_name(*m))
                    } else {
                        view.peer_name(*m)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        });
        println!("Vocal « {} » : {}", c.name, members.unwrap_or_else(|| "vide".into()));
    }
}

fn print_event(ev: &Event, view: &mut ServerView, channel: ChannelId) {
    if let Some(line) = view.apply(ev) {
        println!("* {line}");
    }
    match ev {
        Event::Text { msg, .. } if msg.body.kind.target().is_some() => {
            if msg.body.channel == channel
                && let Some(action) = action(view, msg)
            {
                println!("* {action}");
            }
        }
        Event::Text { msg, .. } if msg.body.channel == channel => println!("{}", format_line(view, msg)),
        Event::Text { msg, .. } => {
            let salon = view
                .card
                .settings()
                .text_channels
                .iter()
                .find(|c| c.id == msg.body.channel)
                .map_or("?", |c| c.name.as_str());
            println!("(#{salon}) {}", format_line(view, msg))
        }
        Event::History(batch) => {
            for m in batch
                .iter()
                .filter(|m| m.body.channel == channel && m.body.kind.target().is_none())
            {
                println!("{}", format_line(view, m));
            }
        }
        Event::Voice(room) => {
            let name = view
                .card
                .settings()
                .voice_channels
                .iter()
                .find(|c| c.id == room.channel)
                .map_or("?", |c| c.name.as_str());
            let members: Vec<_> = room.members.iter().map(|m| view.peer_name(*m)).collect();
            let host = room.host.map(|h| view.peer_name(h)).unwrap_or_default();
            println!("* vocal « {name} » : {} (hôte : {host})", members.join(", "));
        }
        Event::Pong { rtt } => println!("pong ({:.2} ms)", rtt.as_secs_f64() * 1000.0),
        Event::Control(c) => println!("* signal non géré : {c:?}"),
        _ => {}
    }
}

/// « <auteur> texte », avec la réponse et les pièces jointes.
fn format_line(view: &ServerView, m: &SignedMessage) -> String {
    let mut out = format!("<{}> {}", view.author(m), view.text(m));
    if let Kind::Rich { reply_to, files, .. } = &m.body.kind {
        if let Some(t) = reply_to.as_ref().and_then(|t| view.message(t)) {
            out = format!("(en réponse à {}) {out}", view.author(t));
        }
        for f in files {
            out.push_str(&format!(" [📎 {}]", f.name));
        }
    }
    if view.edited(m) {
        out.push_str(" (modifié)");
    }
    out
}

/// Une réaction, une modification ou une suppression, en une ligne.
fn action(view: &ServerView, m: &SignedMessage) -> Option<String> {
    let who = view.author(m);
    let target = view.message(&m.body.kind.target()?);
    let about = target.map_or_else(String::new, |t| format!(" : « {} »", view.text(t)));
    match &m.body.kind {
        Kind::React { emoji, on: true, .. } => Some(format!("{who} a réagi {emoji}{about}")),
        Kind::Edit { .. } => target.map(|t| format_line(view, t)),
        Kind::Delete { .. } => Some(format!("{who} a supprimé un message")),
        _ => None,
    }
}
