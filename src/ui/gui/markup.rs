//! Mise en forme légère des messages : `**gras**`, `*italique*` (ou
//! `_italique_`), `~~barré~~`, `` `code` ``, blocs ```` ``` ````, liens
//! `http(s)://` cliquables et « @mentions ».
//!
//! Volontairement pas de lien masqué (`[texte](adresse)`) : ce qu'on lit est
//! toujours l'adresse qui s'ouvrira.

/// Un morceau de message d'un seul style.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub code: bool,
    /// `text` est une adresse à ouvrir.
    pub link: bool,
    /// `text` est un « @pseudo ».
    pub mention: bool,
}

#[derive(Clone, Copy, Default)]
struct Style {
    bold: bool,
    /// Italique ouvert par ce caractère (`*` ou `_`), qui seul le referme.
    italic: Option<char>,
    strike: bool,
}

impl Style {
    fn span(self, text: String) -> Span {
        Span {
            text,
            bold: self.bold,
            italic: self.italic.is_some(),
            strike: self.strike,
            ..Span::default()
        }
    }
}

const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Découpe un message en morceaux stylés. Le texte des morceaux mis bout à
/// bout redonne le message, sans les marqueurs qui ont servi.
pub fn parse(text: &str) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut style = Style::default();
    let mut buf = String::new();
    let mut prev: Option<char> = None;
    let mut i = 0;
    let flush = |buf: &mut String, style: Style, out: &mut Vec<Span>| {
        if !buf.is_empty() {
            out.push(style.span(std::mem::take(buf)));
        }
    };
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().expect("reste non vide");
        // Début de mot : rien, une espace ou de la ponctuation juste avant.
        let boundary = prev.is_none_or(|p| !p.is_alphanumeric());

        // Bloc de code : tout est pris tel quel jusqu'à la fermeture.
        if let Some(inner) = rest.strip_prefix("```")
            && let Some(close) = inner.find("```")
            && !inner[..close].trim().is_empty()
        {
            flush(&mut buf, style, &mut out);
            out.push(Span {
                text: inner[..close].trim_matches('\n').to_string(),
                code: true,
                ..Span::default()
            });
            i += close + 6;
            prev = Some('`');
            continue;
        }
        if c == '`'
            && let Some(close) = rest[1..].find('`')
            && !rest[1..=close].trim().is_empty()
            && !rest[1..=close].contains('\n')
        {
            flush(&mut buf, style, &mut out);
            out.push(Span {
                text: rest[1..=close].to_string(),
                code: true,
                ..Span::default()
            });
            i += close + 2;
            prev = Some('`');
            continue;
        }

        // Lien : jusqu'à la prochaine espace, sans la ponctuation finale.
        if boundary
            && let Some(scheme) = SCHEMES.iter().find(|s| starts_with_ignore_case(rest, s))
            && let Some(url) = link_at(rest, scheme.len())
        {
            flush(&mut buf, style, &mut out);
            out.push(Span {
                link: true,
                ..style.span(url.to_string())
            });
            i += url.len();
            prev = url.chars().last();
            continue;
        }

        // Mention : « @mot », sans la ponctuation qui le suit.
        if c == '@' && prev.is_none_or(|p| !p.is_alphanumeric()) {
            let word = &rest[1..];
            let end = word.find(char::is_whitespace).unwrap_or(word.len());
            let name = word[..end].trim_end_matches(|c: char| !c.is_alphanumeric());
            if !name.is_empty() {
                flush(&mut buf, style, &mut out);
                out.push(Span {
                    mention: true,
                    ..style.span(rest[..1 + name.len()].to_string())
                });
                i += 1 + name.len();
                prev = name.chars().last();
                continue;
            }
        }

        // Marqueurs de style.
        let mut next = style;
        if let Some(len) = marker(rest, prev, &mut next) {
            flush(&mut buf, style, &mut out);
            style = next;
            i += len;
            prev = Some(c);
            continue;
        }

        buf.push(c);
        prev = Some(c);
        i += c.len_utf8();
    }
    flush(&mut buf, style, &mut out);
    out
}

/// Si `rest` commence par un marqueur qui ouvre ou ferme un style, l'applique
/// à `style` et renvoie sa longueur.
fn marker(rest: &str, prev: Option<char>, style: &mut Style) -> Option<usize> {
    let after = |len: usize| rest[len..].chars().next();
    let closes = prev.is_some_and(|p| !p.is_whitespace());
    let opens = |m: &str| after(m.len()).is_some_and(|a| !a.is_whitespace()) && closing(&rest[m.len()..], m).is_some();
    for m in ["**", "~~"] {
        if !rest.starts_with(m) {
            continue;
        }
        let on = if m == "**" { &mut style.bold } else { &mut style.strike };
        if *on && closes {
            *on = false;
            return Some(m.len());
        }
        if !*on && opens(m) {
            *on = true;
            return Some(m.len());
        }
        // Deux étoiles qui n'ouvrent ni ne ferment rien : du texte.
        return None;
    }
    let c = rest.chars().next()?;
    if c != '*' && c != '_' {
        return None;
    }
    // Un tiret bas au milieu d'un mot (nom_de_fichier) n'est pas de l'italique.
    let inside_word =
        c == '_' && (prev.is_some_and(char::is_alphanumeric) && after(1).is_some_and(char::is_alphanumeric));
    if inside_word {
        return None;
    }
    match style.italic {
        Some(open) if open == c && closes && (c == '*' || !after(1).is_some_and(char::is_alphanumeric)) => {
            style.italic = None;
            Some(1)
        }
        None if (c == '*' || prev.is_none_or(|p| !p.is_alphanumeric())) && opens(&rest[..1]) => {
            style.italic = Some(c);
            Some(1)
        }
        _ => None,
    }
}

/// Où se referme le style ouvert par `m`, dans ce qui suit l'ouverture : le
/// marqueur doit coller au texte qui le précède, sur la même ligne ou plus loin.
fn closing(after_open: &str, m: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(at) = after_open[from..].find(m).map(|p| p + from) {
        let before = after_open[..at].chars().next_back();
        let next = after_open[at + m.len()..].chars().next();
        let glued = before.is_some_and(|b| !b.is_whitespace());
        let word_end = m != "_" || !next.is_some_and(char::is_alphanumeric);
        // « * » seul : pas le début d'un « ** ».
        let single = m != "*" || (next != Some('*') && before != Some('*'));
        if at > 0 && glued && word_end && single {
            return Some(at);
        }
        from = at + m.len();
    }
    None
}

fn starts_with_ignore_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.is_char_boundary(prefix.len()) && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// L'adresse qui commence `rest` : jusqu'à la première espace, moins la
/// ponctuation de fin de phrase et les parenthèses qui l'entouraient.
fn link_at(rest: &str, scheme: usize) -> Option<&str> {
    let end = rest
        .find(|c: char| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '`'))
        .unwrap_or(rest.len());
    let mut url = &rest[..end];
    while let Some(last) = url.chars().next_back() {
        let unbalanced = |open: char, close: char| url.matches(close).count() > url.matches(open).count();
        let trim = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '_' | '~' | '»' | '…' => true,
            ')' => unbalanced('(', ')'),
            ']' => unbalanced('[', ']'),
            _ => false,
        };
        if !trim {
            break;
        }
        url = &url[..url.len() - last.len_utf8()];
    }
    // Il faut au moins un nom après « https:// ».
    let host = &url[scheme.min(url.len())..];
    host.chars().next().is_some_and(char::is_alphanumeric).then_some(url)
}

/// Adresse sûre à ouvrir dans le navigateur : http(s) seulement, sans
/// caractère de contrôle ni sens d'écriture inversé.
pub fn safe_url(url: &str) -> bool {
    SCHEMES.iter().any(|s| starts_with_ignore_case(url, s))
        && url.len() <= 2048
        && !url.chars().any(|c| {
            c.is_whitespace() || c.is_control() || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Span {
        Span {
            text: text.into(),
            ..Span::default()
        }
    }

    fn texts(spans: &[Span]) -> Vec<&str> {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn texte_simple_inchange() {
        assert_eq!(parse("salut tout le monde"), vec![plain("salut tout le monde")]);
        assert_eq!(parse(""), Vec::<Span>::new());
        // Rien à fermer : les marqueurs restent du texte.
        assert_eq!(parse("2 * 3 * 4 = 24"), vec![plain("2 * 3 * 4 = 24")]);
        assert_eq!(parse("mon_fichier_final.txt"), vec![plain("mon_fichier_final.txt")]);
        assert_eq!(parse("**pas fermé"), vec![plain("**pas fermé")]);
        assert_eq!(parse("un ` tout seul"), vec![plain("un ` tout seul")]);
        assert_eq!(parse("a ** b ** c"), vec![plain("a ** b ** c")]);
    }

    #[test]
    fn gras_italique_barre() {
        let s = parse("un **mot** en gras");
        assert_eq!(texts(&s), ["un ", "mot", " en gras"]);
        assert!(s[1].bold && !s[0].bold && !s[2].bold);

        let s = parse("*penché* et _aussi_");
        assert_eq!(texts(&s), ["penché", " et ", "aussi"]);
        assert!(s[0].italic && !s[1].italic && s[2].italic);

        let s = parse("~~raté~~ réussi");
        assert_eq!(texts(&s), ["raté", " réussi"]);
        assert!(s[0].strike && !s[1].strike);

        // Styles imbriqués.
        let s = parse("**gras et *italique* ensemble**");
        assert_eq!(texts(&s), ["gras et ", "italique", " ensemble"]);
        assert!(s.iter().all(|x| x.bold));
        assert!(s[1].italic && !s[0].italic);
    }

    #[test]
    fn code_pris_tel_quel() {
        let s = parse("tape `cargo **run**` ici");
        assert_eq!(texts(&s), ["tape ", "cargo **run**", " ici"]);
        assert!(s[1].code && !s[1].bold);

        let s = parse("```\nfn main() {}\n*pas italique*\n```");
        assert_eq!(s.len(), 1);
        assert!(s[0].code);
        assert_eq!(s[0].text, "fn main() {}\n*pas italique*");
    }

    #[test]
    fn liens_reconnus() {
        let s = parse("regarde https://exemple.fr/page?a=1&b=2, c'est bien.");
        assert_eq!(
            texts(&s),
            ["regarde ", "https://exemple.fr/page?a=1&b=2", ", c'est bien."]
        );
        assert!(s[1].link);

        // Parenthèses : celles de l'adresse restent, celle de la phrase non.
        let s = parse("(voir https://fr.wikipedia.org/wiki/Rust_(langage))");
        assert_eq!(s[1].text, "https://fr.wikipedia.org/wiki/Rust_(langage)");
        assert!(s[1].link);

        // Les tirets bas et étoiles d'une adresse ne la découpent pas.
        let s = parse("https://exemple.fr/un_nom_long*x");
        assert_eq!(s.len(), 1);
        assert!(s[0].link && !s[0].italic);

        // Majuscules acceptées, autres protocoles non.
        assert!(parse("HTTPS://EXEMPLE.FR")[0].link);
        assert!(
            parse("file:///etc/passwd javascript:alert(1) ftp://x")
                .iter()
                .all(|s| !s.link)
        );
        // Collé à un mot, ou sans rien derrière : pas un lien.
        assert!(parse("xhttps://exemple.fr").iter().all(|s| !s.link));
        assert!(parse("https:// rien").iter().all(|s| !s.link));

        // Un lien en gras reste entier.
        let s = parse("**https://exemple.fr**");
        assert_eq!(texts(&s), ["https://exemple.fr"]);
        assert!(s[0].link && s[0].bold);
    }

    #[test]
    fn mentions_reconnues() {
        let s = parse("salut @Lwrsk, ça va ? et @tous !");
        assert_eq!(texts(&s), ["salut ", "@Lwrsk", ", ça va ? et ", "@tous", " !"]);
        assert!(s[1].mention && s[3].mention && !s[0].mention);
        // Une adresse e-mail n'est pas une mention.
        assert!(parse("ecris a moi@exemple.fr").iter().all(|s| !s.mention));
        assert!(parse("@ seul").iter().all(|s| !s.mention));
    }

    #[test]
    fn le_texte_est_conserve() {
        // Sans marqueur reconnu, rien n'est perdu ni déplacé.
        for text in [
            "été à Noël… 100 % d'accord",
            "émoji 🎉 et accents éèà",
            "a*b",
            "_",
            "**",
            "``",
            "``` ```",
            "ligne 1\nligne 2\n\nligne 4",
        ] {
            let joined: String = parse(text).iter().map(|s| s.text.as_str()).collect();
            assert_eq!(joined, text, "{text:?}");
        }
    }

    #[test]
    fn adresses_sures() {
        assert!(safe_url("https://exemple.fr/a?b=c#d"));
        assert!(safe_url("http://203.0.113.5:8080/"));
        assert!(!safe_url("javascript:alert(1)"));
        assert!(!safe_url("file:///C:/Windows/system32/calc.exe"));
        assert!(!safe_url("https://exemple.fr/\u{202E}gpj.exe"));
        assert!(!safe_url("https://exemple.fr/a b"));
    }
}
