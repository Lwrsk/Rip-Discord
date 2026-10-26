//! Interface utilisateur : graphique ([`gui`], par défaut) ou terminal
//! ([`terminal`]). Les deux consomment la même API :
//! [`crate::client::Client`] + [`crate::client::Event`].

pub mod gui;
pub mod terminal;
