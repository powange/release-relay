//! Configuration : fichier TOML monté dans le conteneur (sans secret ; le jeton GitHub est
//! donné à part, par la variable `GITHUB_TOKEN`).
//!
//! ```toml
//! public_url = "https://releases.exemple.fr"
//! cache_ttl = 60      # facultatif : secondes de cache des réponses GitHub
//! proxy_hops = 1      # facultatif : proxys devant le serveur (0 : joint directement)
//! rate_limit = 60     # facultatif : requêtes par minute et par adresse (0 : sans limite)
//!
//! [apps.twitch-desk]
//! repo = "powange/twitch-desk"
//! raw = ["operations.json"]
//!
//! [apps.twitch-desk.site]   # facultatif
//! dir = "site"
//! source = "release"        # ou "branch"
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;

use crate::paths;

const DEFAULT_API: &str = "https://api.github.com";
/// Hôtes vers lesquels l'API des assets redirige (adresses signées et temporaires)
const DEFAULT_DOWNLOAD_ORIGINS: &[&str] =
    &["https://release-assets.githubusercontent.com", "https://objects.githubusercontent.com"];
/// Noms réservés aux routes du serveur
const RESERVED: &[&str] = &["health"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    public_url: String,
    cache_ttl: Option<u64>,
    proxy_hops: Option<usize>,
    rate_limit: Option<u32>,
    github: Option<RawGithub>,
    #[serde(default)]
    apps: BTreeMap<String, RawApp>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGithub {
    api_url: Option<String>,
    download_origins: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawApp {
    repo: String,
    #[serde(default)]
    raw: Vec<String>,
    site: Option<RawSite>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSite {
    dir: String,
    #[serde(default)]
    source: SiteSource,
}

/// Version lue pour la page de présentation
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SiteSource {
    /// Tag de la dernière release : la page décrit la version téléchargeable
    #[default]
    Release,
    /// Branche par défaut du dépôt
    Branch,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Adresse publique, sans `/` final
    pub public_url: String,
    pub cache_ttl: Duration,
    pub proxy_hops: usize,
    /// Requêtes par minute et par adresse ; 0 : sans limite
    pub rate_limit: u32,
    /// Adresse de l'API GitHub, sans `/` final (réglable pour les tests)
    pub api_url: String,
    /// Origines (`https://hôte`) acceptées comme destination d'une redirection de l'API
    pub download_origins: Vec<String>,
    pub apps: BTreeMap<String, App>,
}

#[derive(Debug, Clone)]
pub struct App {
    pub owner: String,
    pub repo: String,
    /// Chemins servis sous `/{app}/raw/` (liste blanche)
    pub raw: BTreeSet<String>,
    pub site: Option<Site>,
}

#[derive(Debug, Clone)]
pub struct Site {
    /// Dossier du dépôt, sans `/` au début ni à la fin
    pub dir: String,
    pub source: SiteSource,
}

/// Adresse http(s) sans requête ni fragment, `/` final retiré
fn base_url(value: &str, what: &str) -> Result<String, String> {
    let value = value.trim().trim_end_matches('/');
    let url = reqwest::Url::parse(value).map_err(|e| format!("{what} : adresse invalide ({e})"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("{what} : adresse http(s) attendue"));
    }
    if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{what} : ni paramètres, ni fragment, ni identifiants dans l'adresse"));
    }
    Ok(value.to_owned())
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| format!("config : {e}"))?;
        let public_url = base_url(&raw.public_url, "public_url")?;
        let github = raw.github.unwrap_or(RawGithub { api_url: None, download_origins: None });
        let api_url = base_url(github.api_url.as_deref().unwrap_or(DEFAULT_API), "github.api_url")?;
        let download_origins = match github.download_origins {
            Some(list) => list
                .iter()
                .map(|o| {
                    let url = reqwest::Url::parse(o.trim()).map_err(|e| format!("github.download_origins : {o} ({e})"))?;
                    if !matches!(url.scheme(), "http" | "https") || url.path() != "/" || url.query().is_some() {
                        return Err(format!("github.download_origins : origine attendue (https://hôte), pas {o}"));
                    }
                    Ok(url.origin().ascii_serialization())
                })
                .collect::<Result<Vec<_>, _>>()?,
            None => DEFAULT_DOWNLOAD_ORIGINS.iter().map(|o| o.to_string()).collect(),
        };
        if raw.apps.is_empty() {
            return Err("config : aucune appli déclarée ([apps.<nom>])".into());
        }
        let mut apps = BTreeMap::new();
        for (name, app) in raw.apps {
            if !paths::valid_app_name(&name) || RESERVED.contains(&name.as_str()) {
                return Err(format!("appli « {name} » : nom invalide (minuscules, chiffres et tirets ; pas « health »)"));
            }
            let (owner, repo) = app
                .repo
                .split_once('/')
                .filter(|(o, r)| paths::valid_repo_part(o) && paths::valid_repo_part(r))
                .ok_or_else(|| format!("appli « {name} » : repo doit être de la forme owner/repo"))?;
            let raw = app
                .raw
                .iter()
                .map(|p| paths::clean_path(p.trim()).ok_or_else(|| format!("appli « {name} » : chemin raw refusé : {p}")))
                .collect::<Result<BTreeSet<_>, _>>()?;
            let site = match app.site {
                Some(s) => {
                    let dir = s.dir.trim().trim_matches('/');
                    let dir = paths::clean_path(dir).ok_or_else(|| format!("appli « {name} » : dossier de page refusé : {}", s.dir))?;
                    Some(Site { dir, source: s.source })
                }
                None => None,
            };
            apps.insert(name, App { owner: owner.to_owned(), repo: repo.to_owned(), raw, site });
        }
        Ok(Config {
            public_url,
            cache_ttl: Duration::from_secs(raw.cache_ttl.unwrap_or(60).max(1)),
            proxy_hops: raw.proxy_hops.unwrap_or(1),
            rate_limit: raw.rate_limit.unwrap_or(60),
            api_url,
            download_origins,
            apps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
        public_url = "https://releases.exemple.fr/"

        [apps.twitch-desk]
        repo = "powange/twitch-desk"
        raw = ["operations.json"]

        [apps.twitch-desk.site]
        dir = "site/"

        [apps.autre]
        repo = "powange/autre"
    "#;

    #[test]
    fn example_with_defaults() {
        let c = Config::from_toml(EXAMPLE).unwrap();
        assert_eq!(c.public_url, "https://releases.exemple.fr");
        assert_eq!(c.cache_ttl, Duration::from_secs(60));
        assert_eq!((c.proxy_hops, c.rate_limit), (1, 60));
        assert_eq!(c.api_url, "https://api.github.com");
        assert_eq!(c.download_origins.len(), 2);
        let td = &c.apps["twitch-desk"];
        assert_eq!((td.owner.as_str(), td.repo.as_str()), ("powange", "twitch-desk"));
        assert!(td.raw.contains("operations.json"));
        let site = td.site.as_ref().unwrap();
        assert_eq!((site.dir.as_str(), site.source), ("site", SiteSource::Release));
        assert!(c.apps["autre"].site.is_none());
        assert!(c.apps["autre"].raw.is_empty());
    }

    #[test]
    fn options() {
        let c = Config::from_toml(
            r#"
            public_url = "http://localhost:8080"
            cache_ttl = 5
            proxy_hops = 0
            rate_limit = 0
            [github]
            api_url = "http://127.0.0.1:9000/"
            download_origins = ["http://127.0.0.1:9000"]
            [apps.a]
            repo = "o/r"
            [apps.a.site]
            dir = "docs/site"
            source = "branch"
            "#,
        )
        .unwrap();
        assert_eq!(c.cache_ttl, Duration::from_secs(5));
        assert_eq!(c.api_url, "http://127.0.0.1:9000");
        assert_eq!(c.download_origins, ["http://127.0.0.1:9000"]);
        assert_eq!(c.apps["a"].site.as_ref().unwrap().source, SiteSource::Branch);
    }

    #[test]
    fn refused() {
        let base = "public_url = \"https://x.fr\"\n";
        for bad in [
            "public_url = \"ftp://x\"\n[apps.a]\nrepo = \"o/r\"",
            "public_url = \"https://x.fr?a=1\"\n[apps.a]\nrepo = \"o/r\"",
            base,
            &format!("{base}[apps.A]\nrepo = \"o/r\""),
            &format!("{base}[apps.health]\nrepo = \"o/r\""),
            &format!("{base}[apps.a]\nrepo = \"o\""),
            &format!("{base}[apps.a]\nrepo = \"o/../r\""),
            &format!("{base}[apps.a]\nrepo = \"o/r\"\nraw = [\"../secret\"]"),
            &format!("{base}[apps.a]\nrepo = \"o/r\"\nraw = [\".env\"]"),
            &format!("{base}[apps.a]\nrepo = \"o/r\"\n[apps.a.site]\ndir = \"../x\""),
            &format!("{base}[apps.a]\nrepo = \"o/r\"\n[apps.a.site]\ndir = \"site\"\nsource = \"tag\""),
            &format!("{base}inconnu = 1\n[apps.a]\nrepo = \"o/r\""),
            &format!("{base}[github]\ndownload_origins = [\"https://h/chemin\"]\n[apps.a]\nrepo = \"o/r\""),
        ] {
            assert!(Config::from_toml(bad).is_err(), "{bad}");
        }
    }
}
