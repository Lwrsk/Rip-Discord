//! Notifications du système : une mention, ou un message pendant que la
//! fenêtre est en arrière-plan.
//!
//! Sous Windows, ce sont les notifications habituelles (en bas à droite, dans
//! le centre de notifications). L'app s'y déclare une fois sous son nom et son
//! icône, sans rien demander : une clé dans le registre de l'utilisateur, pas
//! de droits administrateur. Ailleurs, rien (la barre des tâches clignote
//! quand même).

#[cfg(windows)]
mod windows {
    use std::sync::OnceLock;

    use tauri_winrt_notification::{Duration, Sound, Toast};

    /// Identifiant sous lequel Windows range nos notifications.
    const APP_ID: &str = "RIPDiscord.App";

    /// Déclare l'app à Windows (nom et icône affichés sur la notification).
    fn register() -> Option<&'static str> {
        static ID: OnceLock<Option<&'static str>> = OnceLock::new();
        *ID.get_or_init(|| {
            let icon = crate::core::store::Store::open_default()
                .ok()
                .map(|s| s.root().join("icon.png"))
                .filter(|p| std::fs::write(p, include_bytes!("../../../assets/icon.png")).is_ok());
            let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
            let key = format!(r"Software\Classes\AppUserModelId\{APP_ID}");
            let registered = hkcu.create_subkey(key).and_then(|(k, _)| {
                k.set_value("DisplayName", &"RIP Discord")?;
                if let Some(icon) = &icon {
                    k.set_value("IconUri", &icon.display().to_string())?;
                }
                Ok(())
            });
            match registered {
                Ok(()) => Some(APP_ID),
                Err(e) => {
                    tracing::warn!("notifications : déclaration impossible : {e}");
                    None
                }
            }
        })
    }

    pub fn show(title: String, body: String) {
        // WinRT peut prendre un instant : pas sur le fil de l'interface.
        std::thread::spawn(move || {
            let id = register().unwrap_or(Toast::POWERSHELL_APP_ID);
            let shown = Toast::new(id)
                .title(&title)
                .text1(&body)
                .sound(Some(Sound::Default))
                .duration(Duration::Short)
                .show();
            if let Err(e) = shown {
                tracing::warn!("notification : {e}");
            }
        });
    }
}

/// Affiche une notification du système.
pub fn show(title: &str, body: &str) {
    #[cfg(windows)]
    windows::show(title.to_string(), body.to_string());
    #[cfg(not(windows))]
    tracing::debug!("notification : {title} — {body}");
}
