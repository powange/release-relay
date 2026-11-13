//! Validation des noms et des chemins reçus dans les adresses ou lus dans la config.
//!
//! Tout ce qui sert à construire une adresse de l'API GitHub passe par ici : pas de `..`, pas
//! de fichier caché, pas de caractère de contrôle ni de barre oblique inverse.

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// Longueur maximale d'un chemin
const MAX_PATH: usize = 512;

/// Caractères encodés dans un segment d'adresse (tout sauf lettres, chiffres et `-._~`)
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b':')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'@')
    .add(b'[')
    .add(b'\\')
    .add(b']')
    .add(b'^')
    .add(b'`')
    .add(b'{')
    .add(b'|')
    .add(b'}')
    .add(b'+')
    .add(b'&')
    .add(b'=')
    .add(b'!')
    .add(b'$')
    .add(b'\'')
    .add(b'(')
    .add(b')')
    .add(b'*')
    .add(b',')
    .add(b';');

/// Segment d'adresse encodé
pub fn encode(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT).to_string()
}

/// Chemin encodé segment par segment (les `/` restent)
pub fn encode_path(path: &str) -> String {
    path.split('/').map(encode).collect::<Vec<_>>().join("/")
}

/// Nom d'appli : minuscules, chiffres et tirets, 64 caractères au plus, sans tiret au début
pub fn valid_app_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Propriétaire ou nom de dépôt GitHub
pub fn valid_repo_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 100
        && part != "."
        && part != ".."
        && part.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Nom simple (tag, fichier d'asset, clé de plateforme) : lettres, chiffres et `._+-`, sans
/// point au début ni `..`
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !name.starts_with('.')
        && !name.contains("..")
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// Segment de chemin acceptable : ni vide, ni `.`, ni `..`, ni caché (point au début), sans
/// caractère de contrôle ni `\`
fn valid_segment(segment: &str) -> bool {
    !segment.is_empty() && !segment.starts_with('.') && !segment.chars().any(|c| c.is_control() || c == '\\')
}

/// Chemin relatif propre (`dossier/fichier.ext`) : chaque segment valide, sans `/` au début ni
/// à la fin. `None` si le chemin est refusé
pub fn clean_path(path: &str) -> Option<String> {
    if path.is_empty() || path.len() > MAX_PATH || path.starts_with('/') || path.ends_with('/') {
        return None;
    }
    path.split('/').all(valid_segment).then(|| path.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names() {
        assert!(valid_app_name("twitch-desk"));
        assert!(valid_app_name("app2"));
        for bad in ["", "-x", "Twitch", "a_b", "a.b", "a/b", "..", &"a".repeat(65)] {
            assert!(!valid_app_name(bad), "{bad}");
        }
    }

    #[test]
    fn repo_parts() {
        assert!(valid_repo_part("powange"));
        assert!(valid_repo_part("twitch-desk.rs_2"));
        for bad in ["", ".", "..", "a/b", "a b", "a%2F"] {
            assert!(!valid_repo_part(bad), "{bad}");
        }
    }

    #[test]
    fn names() {
        assert!(valid_name("v0.48.86"));
        assert!(valid_name("Twitch.Desk_0.48.86_x64-setup.exe"));
        assert!(valid_name("windows-x86_64"));
        for bad in ["", ".hidden", "a..b", "..", "a/b", "a\\b", "a b", "a%2e", "é"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn paths() {
        assert_eq!(clean_path("operations.json").as_deref(), Some("operations.json"));
        assert_eq!(clean_path("docs/img/a b.png").as_deref(), Some("docs/img/a b.png"));
        for bad in [
            "",
            "/etc/passwd",
            "../secret",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            ".git/config",
            "a/.env",
            "a\\..\\b",
            "a\0b",
            "a\nb",
        ] {
            assert_eq!(clean_path(bad), None, "{bad:?}");
        }
        assert_eq!(clean_path(&"a/".repeat(300)), None);
    }

    #[test]
    fn encoding() {
        assert_eq!(encode("a b/c?d#e"), "a%20b%2Fc%3Fd%23e");
        assert_eq!(encode("v1.0-rc_2~x"), "v1.0-rc_2~x");
        assert_eq!(encode_path("site/a b.html"), "site/a%20b.html");
    }
}
