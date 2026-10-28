//! Mises à jour : on lit la dernière Release du dépôt public sur GitHub, on
//! télécharge son installeur, on le vérifie, puis on le lance (Windows).
//!
//! Pas de serveur à nous : GitHub publie déjà tout. L'installeur n'est gardé
//! que s'il a exactement le nom, la taille et l'empreinte SHA-256 que l'API
//! annonce (GitHub la calcule lui-même à l'envoi du fichier), qu'il vient du
//! dépôt de l'app, en HTTPS, et que sa version est plus récente que la nôtre.

use std::{
    cmp::Ordering,
    fmt,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use ureq::tls::{RootCerts, TlsConfig};

use crate::core::VERSION;

/// Dépôt dont les Releases font foi.
pub const REPO: &str = "Lwrsk/Rip-Discord";
/// Taille au-delà de laquelle on n'y croit pas (l'installeur fait ~7 Mo).
const MAX_SIZE: u64 = 200 << 20;

/// Version `x.y.z` (sans suffixe : les préversions ne sont pas proposées).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// `v0.1.2` ou `0.1.2`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split('.').map(|p| {
            (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| p.parse().ok())
                .flatten()
        });
        let v = Self(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(v)
    }

    /// La version de cette app.
    pub fn current() -> Self {
        Self::parse(VERSION).unwrap_or(Self(0, 0, 0))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Une Release publiée.
#[derive(Clone, Debug)]
pub struct Release {
    pub version: Version,
    /// Notes de version (Markdown de GitHub).
    pub notes: String,
    /// Page de la Release, à ouvrir dans le navigateur.
    pub page: String,
    /// Installeur Windows vérifiable, s'il y en a un.
    pub installer: Option<Installer>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Installer {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// Où chercher. Toujours GitHub, sauf dans les tests.
#[derive(Clone)]
pub struct Source {
    /// Adresse de la dernière Release dans l'API.
    latest: String,
    /// Début obligatoire de l'adresse de l'installeur.
    downloads: String,
    https_only: bool,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            latest: format!("https://api.github.com/repos/{REPO}/releases/latest"),
            downloads: format!("https://github.com/{REPO}/releases/download/"),
            https_only: true,
        }
    }
}

impl Source {
    /// Serveur de test en clair (`http://127.0.0.1:port`).
    #[doc(hidden)]
    pub fn local(base: &str) -> Self {
        Self {
            latest: format!("{base}/latest"),
            downloads: format!("{base}/download/"),
            https_only: false,
        }
    }

    fn agent(&self, body: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .https_only(self.https_only)
            .user_agent(format!("rip-discord/{VERSION}"))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_recv_body(Some(body))
            .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
            .build()
            .into()
    }

    /// Dernière Release publiée (ni brouillon ni préversion).
    pub fn latest(&self) -> Result<Release> {
        let json = self
            .agent(Duration::from_secs(30))
            .get(&self.latest)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .call()
            .context("GitHub ne répond pas")?
            .into_body()
            .with_config()
            .limit(1 << 20)
            .read_to_string()
            .context("réponse de GitHub illisible")?;
        self.parse(&json)
    }

    /// Lit la réponse de l'API. Un installeur douteux est écarté (la Release
    /// reste proposée, à installer à la main depuis sa page).
    pub fn parse(&self, json: &str) -> Result<Release> {
        #[derive(Deserialize)]
        struct Api {
            tag_name: String,
            html_url: String,
            #[serde(default)]
            body: Option<String>,
            #[serde(default)]
            draft: bool,
            #[serde(default)]
            prerelease: bool,
            #[serde(default)]
            assets: Vec<Asset>,
        }
        #[derive(Deserialize)]
        struct Asset {
            name: String,
            size: u64,
            browser_download_url: String,
            #[serde(default)]
            state: String,
            #[serde(default)]
            digest: Option<String>,
        }

        let api: Api = serde_json::from_str(json).context("réponse de GitHub illisible")?;
        ensure!(!api.draft && !api.prerelease, "pas de version publiée");
        let version =
            Version::parse(&api.tag_name).with_context(|| format!("version « {} » illisible", api.tag_name))?;
        let wanted = format!("RIP-Discord-Setup-{version}.exe");
        let url = format!("{}{}/{wanted}", self.downloads, api.tag_name);
        let installer = api
            .assets
            .into_iter()
            .filter(|a| {
                a.name == wanted
                    && a.state == "uploaded"
                    && (1..=MAX_SIZE).contains(&a.size)
                    && a.browser_download_url == url
            })
            .find_map(|a| {
                Some(Installer {
                    sha256: hex32(a.digest.as_deref()?.strip_prefix("sha256:")?)?,
                    name: a.name,
                    url: a.browser_download_url,
                    size: a.size,
                })
            });
        Ok(Release {
            version,
            notes: api.body.unwrap_or_default(),
            page: api.html_url,
            installer,
        })
    }

    /// GET qui suit lui-même les redirections de GitHub (vers le stockage des
    /// fichiers), seulement vers ses domaines et sans lire le corps des
    /// réponses intermédiaires : derrière certains proxys, ureq bute sur ce
    /// corps vide.
    fn fetch(&self, url: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let agent = self.agent(Duration::from_secs(600));
        let mut url = url.to_string();
        for _ in 0..5 {
            let response = agent.get(&url).config().max_redirects(0).build().call()?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            let next = response
                .headers()
                .get("location")
                .and_then(|l| l.to_str().ok())
                .context("redirection sans adresse")?;
            ensure!(self.may_follow(next), "redirection vers un site inconnu");
            url = next.to_string();
        }
        bail!("trop de redirections")
    }

    /// GitHub et son stockage de fichiers, en HTTPS.
    fn may_follow(&self, url: &str) -> bool {
        if !self.https_only {
            return true;
        }
        let Some(rest) = url.strip_prefix("https://") else {
            return false;
        };
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        host == "github.com"
            || host
                .strip_suffix(".githubusercontent.com")
                .is_some_and(|h| !h.is_empty() && !h.contains(['@', ':']))
    }

    /// Télécharge l'installeur dans `dir` et le vérifie. `progress` reçoit
    /// les octets reçus. Renvoie le chemin du fichier, gardé seulement s'il
    /// est conforme.
    pub fn download(&self, installer: &Installer, dir: &Path, mut progress: impl FnMut(u64)) -> Result<PathBuf> {
        ensure!(
            installer.url.starts_with(&self.downloads) && !installer.name.contains(['/', '\\']),
            "installeur d'origine inconnue"
        );
        std::fs::create_dir_all(dir)?;
        let path = dir.join(&installer.name);
        let part = dir.join(format!("{}.part", installer.name));
        let result = (|| {
            let response = self.fetch(&installer.url).context("téléchargement impossible")?;
            let mut body = response
                .into_body()
                .into_with_config()
                .limit(installer.size + 1)
                .reader();
            let mut file = std::fs::File::create(&part)?;
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            let mut buf = vec![0; 64 * 1024];
            let mut done = 0u64;
            loop {
                let n = body.read(&mut buf).context("téléchargement interrompu")?;
                if n == 0 {
                    break;
                }
                done += n as u64;
                ensure!(done <= installer.size, "fichier plus gros qu'annoncé");
                hash.update(&buf[..n]);
                file.write_all(&buf[..n])?;
                progress(done);
            }
            file.sync_all()?;
            drop(file);
            ensure!(
                done == installer.size,
                "fichier incomplet ({done} octets sur {})",
                installer.size
            );
            ensure!(
                hash.finish().as_ref() == installer.sha256,
                "empreinte SHA-256 différente de celle annoncée par GitHub"
            );
            std::fs::rename(&part, &path)?;
            Ok(path.clone())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        result
    }
}

/// Plus récente que la nôtre ?
pub fn is_newer(release: &Release) -> bool {
    release.version.cmp(&Version::current()) == Ordering::Greater
}

/// Dossier où poser l'installeur téléchargé.
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("RIP Discord")
}

/// Lance l'installeur téléchargé, sans fenêtres (`/S`) ; il attend que l'app
/// soit fermée, remplace l'exe puis la relance (`/relance`). Windows demande
/// l'accord administrateur. Bloque tant qu'il n'a pas été donné.
#[cfg(windows)]
pub fn launch(installer: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let file = wide(installer.as_os_str());
    let verb = wide("open".as_ref());
    let args = wide("/S /relance".as_ref());
    // ShellExecute (et non CreateProcess) : lui sait demander l'élévation
    // qu'exige l'installeur.
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            args.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if code <= 32 {
        bail!("installation annulée (code {code})");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn launch(_installer: &Path) -> Result<()> {
    bail!("l'installation automatique n'existe que sous Windows")
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    let s = s.as_bytes();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0; 32];
    for (i, pair) in s.as_chunks::<2>().0.iter().enumerate() {
        let digit = |c: u8| (c as char).to_digit(16);
        out[i] = (digit(pair[0])? * 16 + digit(pair[1])?) as u8;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "55e1c1089afc7cc9ace137feb341879890ed3566ef5495773186b0bbeb018a4d";

    fn api(tag: &str, name: &str, url: &str, digest: Option<&str>) -> String {
        serde_json::json!({
            "tag_name": tag,
            "html_url": format!("https://github.com/{REPO}/releases/tag/{tag}"),
            "body": "Notes",
            "draft": false,
            "prerelease": false,
            "assets": [{
                "name": name,
                "size": 7337238,
                "state": "uploaded",
                "browser_download_url": url,
                "digest": digest,
            }],
        })
        .to_string()
    }

    #[test]
    fn versions() {
        assert_eq!(Version::parse("v0.1.2"), Some(Version(0, 1, 2)));
        assert_eq!(Version::parse("10.20.30"), Some(Version(10, 20, 30)));
        for bad in ["", "v", "0.1", "0.1.2.3", "0.1.2-rc1", "0.-1.2", "0.1.x", "0..1"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
        assert_eq!(Version::current().to_string(), VERSION);
    }

    #[test]
    fn release_github() {
        let source = Source::default();
        let url = format!("https://github.com/{REPO}/releases/download/v0.2.0/RIP-Discord-Setup-0.2.0.exe");
        let r = source
            .parse(&api(
                "v0.2.0",
                "RIP-Discord-Setup-0.2.0.exe",
                &url,
                Some(&format!("sha256:{DIGEST}")),
            ))
            .unwrap();
        assert_eq!(r.version, Version(0, 2, 0));
        let i = r.installer.unwrap();
        assert_eq!(i.url, url);
        assert_eq!(i.size, 7337238);
        assert_eq!(i.sha256[..2], [0x55, 0xe1]);
    }

    #[test]
    fn installeur_douteux_ecarte() {
        let source = Source::default();
        let good = format!("https://github.com/{REPO}/releases/download/v0.2.0/RIP-Discord-Setup-0.2.0.exe");
        let digest = format!("sha256:{DIGEST}");
        let cases = [
            // Sans empreinte, rien à vérifier.
            api("v0.2.0", "RIP-Discord-Setup-0.2.0.exe", &good, None),
            api("v0.2.0", "RIP-Discord-Setup-0.2.0.exe", &good, Some("sha1:abcd")),
            // Nom d'une autre version.
            api("v0.2.0", "RIP-Discord-Setup-0.1.0.exe", &good, Some(&digest)),
            // Hébergé ailleurs.
            api(
                "v0.2.0",
                "RIP-Discord-Setup-0.2.0.exe",
                "https://example.com/RIP-Discord-Setup-0.2.0.exe",
                Some(&digest),
            ),
            api(
                "v0.2.0",
                "RIP-Discord-Setup-0.2.0.exe",
                &format!("http://github.com/{REPO}/releases/download/v0.2.0/RIP-Discord-Setup-0.2.0.exe"),
                Some(&digest),
            ),
        ];
        for json in cases {
            let r = source.parse(&json).unwrap();
            assert!(r.installer.is_none(), "{json}");
        }
        let pre = api("v0.2.0", "x", &good, None).replace("\"prerelease\":false", "\"prerelease\":true");
        assert!(source.parse(&pre).is_err());
    }

    #[test]
    fn redirections_github_seulement() {
        let s = Source::default();
        assert!(s.may_follow("https://release-assets.githubusercontent.com/github-production-release-asset/1?sig=x"));
        assert!(s.may_follow("https://objects.githubusercontent.com/x"));
        assert!(s.may_follow("https://github.com/x"));
        for bad in [
            "http://release-assets.githubusercontent.com/x",
            "https://evil.com/x.githubusercontent.com",
            "https://githubusercontent.com.evil.com/x",
            "https://x@evil.githubusercontent.com:443/",
            "https://.githubusercontent.com/x",
            "https://example.com/",
        ] {
            assert!(!s.may_follow(bad), "{bad}");
        }
    }

    /// Petit serveur HTTP local qui sert la Release puis l'installeur.
    fn serve(json: String, file: Vec<u8>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let here = base.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(8) {
                let mut stream = stream.unwrap();
                let mut req = [0; 2048];
                let n = stream.read(&mut req).unwrap();
                let req = String::from_utf8_lossy(&req[..n]);
                let path = req.split(' ').nth(1).unwrap_or_default();
                // Comme GitHub : l'installeur est servi après une redirection,
                // dont le corps (vide) est découpé en morceaux.
                if path.starts_with("/download/") && !path.ends_with("?ici") {
                    let head = format!(
                        "HTTP/1.1 302 Found\r\nLocation: {here}{path}?ici\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n"
                    );
                    let _ = stream.write_all(head.as_bytes());
                    continue;
                }
                let body: &[u8] = if path == "/latest" { json.as_bytes() } else { &file };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        base
    }

    #[test]
    fn telechargement_verifie() {
        let file = b"installeur de test".repeat(1000);
        let digest = ring::digest::digest(&ring::digest::SHA256, &file);
        let hex: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("rip-maj-{}", std::process::id()));
        let name = "RIP-Discord-Setup-9.0.0.exe";

        // Bonne empreinte : le fichier est gardé.
        let base = serve(String::new(), file.clone());
        let source = Source::local(&base);
        let url = format!("{base}/download/v9.0.0/{name}");
        let json =
            api("v9.0.0", name, &url, Some(&format!("sha256:{hex}"))).replace("7337238", &file.len().to_string());
        let release = source.parse(&json).unwrap();
        assert!(is_newer(&release));
        let installer = release.installer.unwrap();
        let mut seen = 0;
        let path = source.download(&installer, &dir, |n| seen = n).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), file);
        assert_eq!(seen, file.len() as u64);

        // Empreinte différente : refusé, rien ne reste.
        std::fs::remove_file(&path).unwrap();
        let base = serve(String::new(), b"autre chose".repeat(1637)[..file.len()].to_vec());
        let source = Source::local(&base);
        let mut other = installer.clone();
        other.url = format!("{base}/download/v9.0.0/{name}");
        let err = source.download(&other, &dir, |_| {}).unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "{err:#}");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());

        // Adresse hors du dépôt : refusé avant même de télécharger.
        other.url = "https://example.com/x.exe".into();
        assert!(source.download(&other, &dir, |_| {}).is_err());

        // Dernière Release lue par l'API (serveur local).
        let base = serve(json, Vec::new());
        assert_eq!(Source::local(&base).latest().unwrap().version, Version(9, 0, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
