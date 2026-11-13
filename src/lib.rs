//! Release Relay : porte d'entrée publique des mises à jour d'applis Tauri dont le dépôt GitHub
//! est privé. Le serveur tient un jeton en lecture seule et ne relaie que ce que la config
//! autorise, pour chaque appli déclarée :
//!
//! - `GET /{app}/latest.json` : manifeste de la dernière release, adresses réécrites vers le relais
//! - `GET /{app}/download/{tag}/{fichier}` : 302 vers l'adresse signée et temporaire de l'asset
//! - `GET /{app}/latest/{plateforme}` : installeur de la dernière version (boutons « Télécharger »)
//! - `GET /{app}/raw/{chemin}` : fichier de la branche par défaut, liste blanche de la config
//! - `GET /{app}/` et `GET /{app}/{chemin}` : page de présentation (dossier du dépôt)
//! - `GET /health`

pub mod config;
pub mod github;
mod limit;
mod paths;
mod raw;
mod site;
mod updater;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::timeout::TimeoutLayer;

use config::{App, Config};
use github::{GhError, GitHub};
use limit::Limiter;

/// Requêtes traitées en même temps (les suivantes attendent leur tour)
const MAX_CONCURRENT: usize = 64;
/// Au-delà, la requête est abandonnée (les appels à GitHub ont leur propre délai, 15 s)
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Relay {
    pub config: Config,
    pub github: GitHub,
    limiter: Limiter,
}

pub type Shared = Arc<Relay>;

impl Relay {
    pub fn new(config: Config, token: &str) -> Result<Shared, String> {
        let github = GitHub::new(&config, token)?;
        let limiter = Limiter::new(config.rate_limit);
        Ok(Arc::new(Relay { config, github, limiter }))
    }

    /// Appli déclarée, sinon 404
    fn app(&self, name: &str) -> Result<&App, RelayError> {
        self.config.apps.get(name).ok_or(RelayError::NotFound)
    }
}

/// Erreur renvoyée au client : rien du détail (journalisé à part), jamais le jeton
#[derive(Debug)]
pub enum RelayError {
    NotFound,
    Upstream(String),
}

impl From<GhError> for RelayError {
    fn from(e: GhError) -> Self {
        match e {
            GhError::NotFound => RelayError::NotFound,
            GhError::Unavailable(e) => RelayError::Upstream(e),
        }
    }
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        match self {
            RelayError::NotFound => not_found(),
            RelayError::Upstream(e) => {
                tracing::warn!("{e}");
                text(StatusCode::BAD_GATEWAY, "GitHub ne répond pas comme prévu, réessayer plus tard\n")
            }
        }
    }
}

fn text(status: StatusCode, body: &'static str) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

fn not_found() -> Response {
    text(StatusCode::NOT_FOUND, "Introuvable\n")
}

/// `/health` : le serveur répond (sans appeler GitHub)
async fn health() -> &'static str {
    "ok\n"
}

/// Limite de requêtes par adresse (voir `limit`)
async fn rate_limit(State(relay): State<Shared>, request: Request, next: Next) -> Response {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip());
    let ip = limit::client_ip(request.headers(), peer, relay.config.proxy_hops);
    match relay.limiter.check(ip, Instant::now()) {
        Ok(()) => next.run(request).await,
        Err(wait) => {
            let mut response = text(StatusCode::TOO_MANY_REQUESTS, "Trop de requêtes, réessayer dans un moment\n");
            let secs = wait.as_secs().max(1).to_string();
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_str(&secs).unwrap());
            response
        }
    }
}

/// En-têtes de sécurité communs
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

pub fn app(relay: Shared) -> Router {
    // Routes prioritaires d'abord (segments fixes : latest.json, download, latest, raw), puis la
    // page de présentation qui prend le reste
    let routes = Router::new()
        .route("/{app}/latest.json", get(updater::manifest))
        .route("/{app}/download/{tag}/{file}", get(updater::download))
        .route("/{app}/latest/{platform}", get(updater::latest))
        .route("/{app}/raw/{*path}", get(raw::raw))
        .route("/{app}", get(site::bare))
        .route("/{app}/", get(site::index))
        .route("/{app}/{*path}", get(site::file))
        .layer(GlobalConcurrencyLimitLayer::new(MAX_CONCURRENT))
        .layer(TimeoutLayer::with_status_code(StatusCode::GATEWAY_TIMEOUT, REQUEST_TIMEOUT))
        .layer(middleware::from_fn_with_state(relay.clone(), rate_limit));
    Router::new()
        .route("/health", get(health))
        .merge(routes)
        .fallback(|| async { not_found() })
        .layer(middleware::from_fn(security_headers))
        .with_state(relay)
}
