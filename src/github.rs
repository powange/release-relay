//! Client de l'API GitHub : tient le jeton, met les réponses en cache et lit les redirections
//! de l'API des assets sans les suivre.
//!
//! - Le jeton n'est envoyé qu'à l'API (`api_url`), jamais aux adresses signées ; il est marqué
//!   sensible et n'apparaît ni dans les journaux ni dans les erreurs.
//! - Réponses mises en cache `cache_ttl` ; si GitHub ne répond plus, la dernière réponse valide
//!   est resservie. Un 404 est gardé `cache_ttl` aussi (pas resservi au-delà).
//! - Contenus immuables (fichier lu par son SHA, contenu d'un asset par son identifiant) : gardés
//!   sans limite de durée, dans une réserve de taille bornée.
//! - Toute requête sortante a un délai d'attente ; les redirections ne sont jamais suivies.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, LOCATION};
use reqwest::{StatusCode, Url};

use crate::config::Config;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Taille maximale d'une réponse lue en entier (JSON de l'API, fichier de la page, raw)
pub const MAX_BODY: usize = 10 * 1024 * 1024;
/// Entrées gardées au plus dans le cache des réponses (au-delà : les périmées sont oubliées,
/// puis tout s'il le faut)
const MAX_ENTRIES: usize = 2_000;
/// Taille maximale de la réserve des contenus immuables (au-delà, elle est vidée)
const MAX_STORE: usize = 64 * 1024 * 1024;

pub const ACCEPT_JSON: &str = "application/vnd.github+json";
pub const ACCEPT_RAW: &str = "application/vnd.github.raw+json";
pub const ACCEPT_OCTET: &str = "application/octet-stream";

/// Erreur vue par les routes. Le détail (sans jeton) va dans les journaux, pas au client
#[derive(Debug)]
pub enum GhError {
    /// GitHub répond 404 (absent, ou invisible avec ce jeton)
    NotFound,
    /// GitHub injoignable, en erreur, ou réponse inattendue
    Unavailable(String),
}

impl std::fmt::Display for GhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GhError::NotFound => write!(f, "introuvable sur GitHub"),
            GhError::Unavailable(e) => write!(f, "{e}"),
        }
    }
}

/// Erreur reqwest sans l'adresse (elle ne contient pas le jeton, mais autant ne garder que la
/// nature de l'erreur)
fn describe(e: reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "délai dépassé"
    } else if e.is_connect() {
        "connexion impossible"
    } else if e.is_body() || e.is_decode() {
        "réponse illisible"
    } else {
        "erreur de requête"
    };
    format!("{kind} ({})", e.without_url())
}

struct Entry {
    at: Instant,
    /// `None` : 404
    body: Option<Bytes>,
}

#[derive(Default)]
struct Store {
    map: HashMap<String, Bytes>,
    size: usize,
}

pub struct GitHub {
    http: reqwest::Client,
    api: String,
    /// `Authorization: Bearer …`, marqué sensible
    auth: HeaderValue,
    download_origins: Vec<String>,
    ttl: Duration,
    cache: Mutex<HashMap<String, Entry>>,
    store: Mutex<Store>,
}

impl GitHub {
    pub fn new(config: &Config, token: &str) -> Result<GitHub, String> {
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| "GITHUB_TOKEN contient des caractères invalides".to_owned())?;
        auth.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert("x-github-api-version", HeaderValue::from_static("2022-11-28"));
        let http = reqwest::Client::builder()
            .user_agent(concat!("release-relay/", env!("CARGO_PKG_VERSION")))
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| format!("client HTTP : {}", describe(e)))?;
        Ok(GitHub {
            http,
            api: config.api_url.clone(),
            auth,
            download_origins: config.download_origins.clone(),
            ttl: config.cache_ttl,
            cache: Mutex::new(HashMap::new()),
            store: Mutex::new(Store::default()),
        })
    }

    /// Requête vers l'API (`path` commence par `/`, déjà encodé), jeton compris
    async fn api_request(&self, path: &str, accept: &str) -> Result<reqwest::Response, GhError> {
        self.http
            .get(format!("{}{path}", self.api))
            .header(AUTHORIZATION, self.auth.clone())
            .header(ACCEPT, accept)
            .send()
            .await
            .map_err(|e| GhError::Unavailable(format!("GitHub {path} : {}", describe(e))))
    }

    /// Corps lu en entier, `MAX_BODY` au plus
    async fn read_body(mut response: reqwest::Response, what: &str) -> Result<Bytes, GhError> {
        if response.content_length().is_some_and(|l| l > MAX_BODY as u64) {
            return Err(GhError::Unavailable(format!("{what} : réponse trop grande")));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| GhError::Unavailable(format!("{what} : {}", describe(e))))? {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(GhError::Unavailable(format!("{what} : réponse trop grande")));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body.into())
    }

    /// GET sur l'API sans cache : corps (200), `NotFound` (404) ou erreur
    async fn fetch(&self, path: &str, accept: &str) -> Result<Bytes, GhError> {
        let response = self.api_request(path, accept).await?;
        match response.status() {
            StatusCode::OK => Self::read_body(response, path).await,
            StatusCode::NOT_FOUND => Err(GhError::NotFound),
            s => Err(GhError::Unavailable(format!("GitHub {path} : statut {s}"))),
        }
    }

    /// GET sur l'API avec cache : réponse de moins de `cache_ttl`, sinon nouvelle requête ;
    /// GitHub en panne : dernière réponse valide resservie
    pub async fn get(&self, path: &str, accept: &str) -> Result<Bytes, GhError> {
        let key = format!("{accept} {path}");
        let stale = {
            let cache = self.cache.lock().unwrap();
            match cache.get(&key) {
                Some(e) if e.at.elapsed() < self.ttl => {
                    return e.body.clone().ok_or(GhError::NotFound);
                }
                Some(e) => e.body.clone(),
                None => None,
            }
        };
        let result = self.fetch(path, accept).await;
        let body = match result {
            Ok(body) => Some(body),
            Err(GhError::NotFound) => None,
            Err(GhError::Unavailable(e)) => {
                return match stale {
                    Some(body) => {
                        tracing::warn!("{e} : dernière réponse valide resservie");
                        Ok(body)
                    }
                    None => Err(GhError::Unavailable(e)),
                };
            }
        };
        let mut cache = self.cache.lock().unwrap();
        if cache.len() >= MAX_ENTRIES && !cache.contains_key(&key) {
            // Les réponses valides sont gardées pour les pannes : seuls les 404 périmés partent
            // d'abord ; tout est oublié si cela ne suffit pas
            cache.retain(|_, e| e.body.is_some() || e.at.elapsed() < self.ttl);
            if cache.len() >= MAX_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(key, Entry { at: Instant::now(), body: body.clone() });
        body.ok_or(GhError::NotFound)
    }

    /// GET sur l'API, JSON décodé (avec cache)
    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, GhError> {
        let body = self.get(path, ACCEPT_JSON).await?;
        serde_json::from_slice(&body).map_err(|e| GhError::Unavailable(format!("GitHub {path} : JSON inattendu ({e})")))
    }

    /// Adresse vers laquelle l'API redirige (sans la suivre) ; refusée si son origine n'est pas
    /// dans `download_origins`. Jamais mise en cache : elle est signée et temporaire
    pub async fn redirect_location(&self, path: &str, accept: &str) -> Result<Url, GhError> {
        let response = self.api_request(path, accept).await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            return Err(GhError::NotFound);
        }
        if !status.is_redirection() {
            return Err(GhError::Unavailable(format!("GitHub {path} : redirection attendue, statut {status}")));
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|l| l.to_str().ok())
            .and_then(|l| Url::parse(l).ok())
            .ok_or_else(|| GhError::Unavailable(format!("GitHub {path} : redirection sans adresse valide")))?;
        if !self.download_allowed(&location) {
            // Seule l'origine est journalisée : la suite de l'adresse porte la signature
            return Err(GhError::Unavailable(format!(
                "GitHub {path} : redirection vers {} refusée (hors de github.download_origins)",
                location.origin().ascii_serialization()
            )));
        }
        Ok(location)
    }

    /// Origine de l'adresse dans la liste des destinations acceptées
    pub fn download_allowed(&self, url: &Url) -> bool {
        let origin = url.origin().ascii_serialization();
        url.username().is_empty() && url.password().is_none() && self.download_origins.contains(&origin)
    }

    /// Contenu immuable (clé stable : SHA d'un fichier, identifiant d'un asset) : réserve, sinon
    /// `load` puis mise en réserve
    async fn immutable<F, Fut>(&self, key: String, load: F) -> Result<Bytes, GhError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes, GhError>>,
    {
        if let Some(body) = self.store.lock().unwrap().map.get(&key) {
            return Ok(body.clone());
        }
        let body = load().await?;
        let mut store = self.store.lock().unwrap();
        if store.size + body.len() > MAX_STORE {
            store.map.clear();
            store.size = 0;
        }
        if let Some(old) = store.map.insert(key, body.clone()) {
            store.size -= old.len();
        }
        store.size += body.len();
        Ok(body)
    }

    /// GET sur l'API d'une réponse qui ne change plus (arbre d'un tag…) : gardée en réserve
    pub async fn get_immutable(&self, path: &str, accept: &str) -> Result<Bytes, GhError> {
        self.immutable(format!("{accept} {path}"), || self.fetch(path, accept)).await
    }

    /// Fichier du dépôt lu par son SHA (`git/blobs`)
    pub async fn blob(&self, owner: &str, repo: &str, sha: &str) -> Result<Bytes, GhError> {
        let path = format!("/repos/{owner}/{repo}/git/blobs/{sha}");
        self.immutable(format!("blob {owner}/{repo} {sha}"), || self.fetch(&path, ACCEPT_RAW)).await
    }

    /// Contenu d'un asset de release : l'API redirige vers une adresse signée, téléchargée ici
    /// **sans** le jeton
    pub async fn asset(&self, owner: &str, repo: &str, id: u64) -> Result<Bytes, GhError> {
        let path = format!("/repos/{owner}/{repo}/releases/assets/{id}");
        self.immutable(format!("asset {owner}/{repo} {id}"), || async {
            let url = self.redirect_location(&path, ACCEPT_OCTET).await?;
            let response = self
                .http
                .get(url)
                .send()
                .await
                .map_err(|e| GhError::Unavailable(format!("asset {id} : {}", describe(e))))?;
            if response.status() != StatusCode::OK {
                return Err(GhError::Unavailable(format!("asset {id} : statut {}", response.status())));
            }
            Self::read_body(response, &format!("asset {id}")).await
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github() -> GitHub {
        let config = Config::from_toml("public_url = \"https://x.fr\"\n[apps.a]\nrepo = \"o/r\"").unwrap();
        GitHub::new(&config, "ghp_secret").unwrap()
    }

    #[test]
    fn token_is_hidden() {
        let g = github();
        assert!(g.auth.is_sensitive());
        assert!(!format!("{:?}", g.auth).contains("ghp_secret"));
    }

    #[test]
    fn only_expected_download_hosts() {
        let g = github();
        for ok in [
            "https://release-assets.githubusercontent.com/github-production-release-asset/1/2?sig=x",
            "https://objects.githubusercontent.com/a",
        ] {
            assert!(g.download_allowed(&Url::parse(ok).unwrap()), "{ok}");
        }
        for bad in [
            "http://release-assets.githubusercontent.com/a",
            "https://release-assets.githubusercontent.com.evil.fr/a",
            "https://evil.fr/release-assets.githubusercontent.com",
            "https://user@release-assets.githubusercontent.com/a",
            "https://release-assets.githubusercontent.com:8443/a",
        ] {
            assert!(!g.download_allowed(&Url::parse(bad).unwrap()), "{bad}");
        }
    }
}
