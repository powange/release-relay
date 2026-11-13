//! Release Relay : serveur (voir `lib.rs` pour les routes).
//!
//! Configuration :
//! - `GITHUB_TOKEN` (obligatoire) : jeton fine-grained en lecture seule (« Contents: read »),
//!   limité aux dépôts relayés ; jamais journalisé ni renvoyé
//! - `RELAY_CONFIG` : fichier TOML (défaut : /config/relay.toml), voir `config.rs`
//! - `RELAY_PORT` : port d'écoute (défaut : 8080)
//!
//! `release-relay health` : vérification de santé pour Docker (sans curl dans l'image).

use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

use release_relay::config::Config;
use release_relay::{app, Relay};

/// `release-relay health` : `/health` répond 200
fn health_check(port: u16) -> ExitCode {
    use std::io::{Read, Write};
    let ok = (|| -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(3))?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        Ok(buf.starts_with("HTTP/1.0 200") || buf.starts_with("HTTP/1.1 200"))
    })()
    .unwrap_or(false);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let port: u16 = std::env::var("RELAY_PORT").ok().and_then(|p| p.trim().parse().ok()).unwrap_or(8080);
    if std::env::args().nth(1).as_deref() == Some("health") {
        return health_check(port);
    }

    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    if token.trim().is_empty() {
        tracing::error!("GITHUB_TOKEN manquant : jeton GitHub fine-grained en lecture seule (Contents: read)");
        return ExitCode::FAILURE;
    }
    let path = std::env::var("RELAY_CONFIG").ok().filter(|p| !p.trim().is_empty()).unwrap_or_else(|| "/config/relay.toml".into());
    let config = match std::fs::read_to_string(&path).map_err(|e| format!("lecture de {path} : {e}")).and_then(|t| Config::from_toml(&t)) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let relay = match Relay::new(config, &token) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };
    drop(token);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("écoute sur {addr} : {e}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        "Release Relay {} sur le port {port}, adresse publique {}, applis : {}",
        env!("CARGO_PKG_VERSION"),
        relay.config.public_url,
        relay.config.apps.keys().cloned().collect::<Vec<_>>().join(", ")
    );

    // Vérification au démarrage : chaque dépôt est visible avec ce jeton (sans bloquer le service)
    let check = relay.clone();
    tokio::spawn(async move {
        for (name, a) in &check.config.apps {
            let path = format!("/repos/{}/{}", a.owner, a.repo);
            match check.github.get(&path, release_relay::github::ACCEPT_JSON).await {
                Ok(_) => tracing::info!("{name} : dépôt {}/{} accessible", a.owner, a.repo),
                Err(e) => tracing::warn!(
                    "{name} : dépôt {}/{} inaccessible ({e}) ; vérifier le nom et les dépôts autorisés du jeton",
                    a.owner,
                    a.repo
                ),
            }
        }
    });

    let stop = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
            tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
        }
        #[cfg(not(unix))]
        let _ = ctrl_c.await;
    };
    let service = app(relay).into_make_service_with_connect_info::<SocketAddr>();
    if let Err(e) = axum::serve(listener, service).with_graceful_shutdown(stop).await {
        tracing::error!("serveur : {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
