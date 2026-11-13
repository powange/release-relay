//! `GET /{app}/raw/{chemin}` : fichier de la branche par défaut, **seulement** pour les chemins
//! listés pour cette appli dans la config (`raw = [...]`).

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};

use crate::github::ACCEPT_RAW;
use crate::paths;
use crate::{RelayError, Shared};

/// Chemin demandé, s'il est dans la liste blanche
pub fn allowed(raw: &std::collections::BTreeSet<String>, path: &str) -> Option<String> {
    paths::clean_path(path).filter(|p| raw.contains(p))
}

/// Type MIME d'après l'extension, avec le jeu de caractères pour le texte
pub fn content_type(path: &str) -> HeaderValue {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let text = mime.type_() == mime_guess::mime::TEXT
        || matches!(mime.essence_str(), "application/json" | "application/javascript" | "application/xml" | "image/svg+xml");
    let value = if text && mime.get_param("charset").is_none() { format!("{mime}; charset=utf-8") } else { mime.to_string() };
    HeaderValue::from_str(&value).unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

pub async fn raw(State(relay): State<Shared>, Path((name, path)): Path<(String, String)>) -> Result<Response, RelayError> {
    let app = relay.app(&name)?;
    let path = allowed(&app.raw, &path).ok_or(RelayError::NotFound)?;
    let api = format!("/repos/{}/{}/contents/{}", app.owner, app.repo, paths::encode_path(&path));
    let body = relay.github.get(&api, ACCEPT_RAW).await?;
    let cache = format!("public, max-age={}", relay.config.cache_ttl.as_secs());
    Ok(([(header::CONTENT_TYPE, content_type(&path)), (header::CACHE_CONTROL, HeaderValue::from_str(&cache).unwrap())], body).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist() {
        let raw = ["operations.json".to_owned(), "docs/a.md".to_owned()].into_iter().collect();
        assert_eq!(allowed(&raw, "operations.json").as_deref(), Some("operations.json"));
        assert_eq!(allowed(&raw, "docs/a.md").as_deref(), Some("docs/a.md"));
        for bad in ["Operations.json", "operations.json/", "./operations.json", "docs/../operations.json", "docs", "README.md", ""] {
            assert_eq!(allowed(&raw, bad), None, "{bad}");
        }
    }

    #[test]
    fn mime_types() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("operations.json"), "application/json; charset=utf-8");
        assert_eq!(content_type("icon.svg"), "image/svg+xml; charset=utf-8");
        assert_eq!(content_type("a.png"), "image/png");
        assert_eq!(content_type("sans-extension"), "application/octet-stream");
    }
}
