//! Cœur réseau partagé par le client et l'hôte.

pub mod card;
pub mod directory;
pub mod files;
pub mod framing;
pub mod identity;
pub mod invite;
pub mod journal;
pub mod limit;
pub mod nat;
pub mod protocol;
pub mod screen;
pub mod store;
pub mod tls;
pub mod voice;

/// Version de l'app (celle de l'installeur).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identifiant ALPN négocié pendant la poignée de main QUIC.
pub const ALPN: &[u8] = b"rip-discord/1";
/// ALPN des connexions directes entre membres d'un salon vocal.
pub const MESH_ALPN: &[u8] = b"rip-discord-mesh/1";

/// Port par défaut de l'hôte.
pub const DEFAULT_PORT: u16 = 7878;
