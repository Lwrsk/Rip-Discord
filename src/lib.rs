//! RIP Discord : chat local et privé à hôte dynamique.
//!
//! Un seul binaire : chaque instance peut être client, hôte, ou les deux
//! (l'hôte est aussi un participant connecté à son propre relais).

pub mod audio;
pub mod capture;
pub mod client;
pub mod core;
pub mod server;
pub mod ui;
