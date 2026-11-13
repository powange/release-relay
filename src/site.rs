//! Page de présentation : fichiers statiques d'un dossier du dépôt (`[apps.x.site]`), pour
//! remplacer GitHub Pages (qui ne publie pas depuis un dépôt privé avec un compte gratuit).
//!
//! - Version lue : tag de la dernière release (par défaut) ou branche par défaut.
//! - Le dossier est listé d'un bloc par l'API des arbres (`git/trees/{ref}:{dossier}`) ; chaque
//!   fichier est lu ensuite par son SHA et gardé en réserve : un fichier inchangé d'une release à
//!   l'autre n'est pas relu. L'arbre d'un tag est gardé jusqu'à la release suivante.
//! - Rien en dehors du dossier, pas de fichier caché ni de lien symbolique ; `index.html` pour
//!   un dossier ; 404 sinon.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::config::{App, Site, SiteSource};
use crate::github::ACCEPT_JSON;
use crate::paths::{self, encode_path};
use crate::raw::content_type;
use crate::updater::latest_release;
use crate::{RelayError, Shared};

#[derive(Debug, Deserialize)]
struct Tree {
    tree: Vec<TreeEntry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct TreeEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    mode: String,
    sha: String,
}

#[derive(Debug, Deserialize)]
struct Repo {
    default_branch: String,
}

/// Fichiers servis : chemin relatif au dossier → SHA. Seuls les fichiers ordinaires aux chemins
/// propres (pas cachés, pas de lien symbolique ni de sous-module)
fn files_of(tree: Tree) -> HashMap<String, String> {
    tree.tree
        .into_iter()
        .filter(|e| e.kind == "blob" && e.mode != "120000")
        .filter(|e| e.sha.len() == 40 && e.sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .filter_map(|e| paths::clean_path(&e.path).map(|p| (p, e.sha)))
        .collect()
}

/// Fichier visé par l'adresse : `None` (racine) ou chemin finissant par `/` → `index.html`
fn requested(path: Option<&str>) -> Option<String> {
    match path {
        None | Some("") => Some("index.html".into()),
        Some(p) => match p.strip_suffix('/') {
            Some(dir) => paths::clean_path(dir).map(|d| format!("{d}/index.html")),
            None => paths::clean_path(p),
        },
    }
}

/// Réponse pour un chemin dans la liste des fichiers : SHA à servir, redirection vers le
/// dossier (`chemin/`) ou rien
#[derive(Debug, PartialEq)]
enum Lookup {
    File(String),
    Dir,
    Missing,
}

fn lookup(files: &HashMap<String, String>, path: &str) -> Lookup {
    if let Some(sha) = files.get(path) {
        Lookup::File(sha.clone())
    } else if files.contains_key(&format!("{path}/index.html")) {
        Lookup::Dir
    } else {
        Lookup::Missing
    }
}

/// Nom de branche acceptable dans une adresse de l'API
fn valid_ref(r: &str) -> bool {
    paths::clean_path(r).is_some_and(|p| p.split('/').all(paths::valid_name))
}

/// Fichiers du dossier pour la version configurée
async fn files(relay: &Shared, app: &App, site: &Site) -> Result<HashMap<String, String>, RelayError> {
    let (git_ref, immutable) = match site.source {
        SiteSource::Release => (latest_release(relay, app).await?.tag_name, true),
        SiteSource::Branch => {
            let repo: Repo = relay.github.get_json(&format!("/repos/{}/{}", app.owner, app.repo)).await?;
            (repo.default_branch, false)
        }
    };
    if !valid_ref(&git_ref) {
        return Err(RelayError::Upstream(format!("{}/{} : référence inattendue {git_ref:?}", app.owner, app.repo)));
    }
    let api = format!("/repos/{}/{}/git/trees/{}:{}?recursive=1", app.owner, app.repo, encode_path(&git_ref), encode_path(&site.dir));
    let body = if immutable {
        relay.github.get_immutable(&api, ACCEPT_JSON).await?
    } else {
        relay.github.get(&api, ACCEPT_JSON).await?
    };
    let tree: Tree = serde_json::from_slice(&body)
        .map_err(|e| RelayError::Upstream(format!("{}/{} : arbre de {git_ref} illisible ({e})", app.owner, app.repo)))?;
    if tree.truncated {
        tracing::warn!("{}/{} : dossier {} trop grand, liste incomplète", app.owner, app.repo, site.dir);
    }
    Ok(files_of(tree))
}

fn moved(location: String) -> Response {
    match HeaderValue::from_str(&location) {
        Ok(l) => (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, l)]).into_response(),
        Err(_) => RelayError::NotFound.into_response(),
    }
}

async fn serve(relay: Shared, name: String, path: Option<String>, headers: HeaderMap) -> Result<Response, RelayError> {
    let app = relay.app(&name)?;
    let site = app.site.as_ref().ok_or(RelayError::NotFound)?;
    let wanted = requested(path.as_deref()).ok_or(RelayError::NotFound)?;
    let files = files(&relay, app, site).await?;
    let sha = match lookup(&files, &wanted) {
        Lookup::File(sha) => sha,
        Lookup::Dir => {
            let location = format!("{}/{name}/{}/", relay.config.public_url, encode_path(&wanted));
            return Ok(moved(location));
        }
        Lookup::Missing => return Err(RelayError::NotFound),
    };
    let etag = format!("\"{sha}\"");
    let etag_value = HeaderValue::from_str(&etag).unwrap();
    let cache = HeaderValue::from_static("no-cache");
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).is_some_and(|v| v.split(',').any(|t| t.trim() == etag)) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag_value), (header::CACHE_CONTROL, cache)]).into_response());
    }
    let body = relay.github.blob(&app.owner, &app.repo, &sha).await?;
    Ok(([(header::CONTENT_TYPE, content_type(&wanted)), (header::ETAG, etag_value), (header::CACHE_CONTROL, cache)], body).into_response())
}

/// `GET /{app}` : vers `/{app}/` (les liens relatifs de la page en dépendent)
pub async fn bare(State(relay): State<Shared>, Path(name): Path<String>) -> Result<Response, RelayError> {
    let app = relay.app(&name)?;
    app.site.as_ref().ok_or(RelayError::NotFound)?;
    let location = format!("{}/{name}/", relay.config.public_url);
    Ok(moved(location))
}

/// `GET /{app}/`
pub async fn index(State(relay): State<Shared>, Path(name): Path<String>, headers: HeaderMap) -> Result<Response, RelayError> {
    serve(relay, name, None, headers).await
}

/// `GET /{app}/{chemin}`
pub async fn file(State(relay): State<Shared>, Path((name, path)): Path<(String, String)>, headers: HeaderMap) -> Result<Response, RelayError> {
    serve(relay, name, Some(path), headers).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, kind: &str, mode: &str) -> TreeEntry {
        TreeEntry { path: path.into(), kind: kind.into(), mode: mode.into(), sha: "a".repeat(40) }
    }

    fn sample() -> HashMap<String, String> {
        files_of(Tree {
            tree: vec![
                entry("index.html", "blob", "100644"),
                entry("icon.svg", "blob", "100644"),
                entry("docs", "tree", "040000"),
                entry("docs/index.html", "blob", "100644"),
                entry(".env", "blob", "100644"),
                entry("img/.cache/x.png", "blob", "100644"),
                entry("lien", "blob", "120000"),
                entry("module", "commit", "160000"),
            ],
            truncated: false,
        })
    }

    #[test]
    fn only_plain_visible_files() {
        let mut names: Vec<_> = sample().into_keys().collect();
        names.sort();
        assert_eq!(names, ["docs/index.html", "icon.svg", "index.html"]);
    }

    #[test]
    fn index_for_folders() {
        assert_eq!(requested(None).as_deref(), Some("index.html"));
        assert_eq!(requested(Some("docs/")).as_deref(), Some("docs/index.html"));
        assert_eq!(requested(Some("icon.svg")).as_deref(), Some("icon.svg"));
        let files = sample();
        assert!(matches!(lookup(&files, "index.html"), Lookup::File(_)));
        assert_eq!(lookup(&files, "docs"), Lookup::Dir);
        assert_eq!(lookup(&files, "absent.html"), Lookup::Missing);
    }

    #[test]
    fn nothing_outside_the_folder() {
        for bad in ["../README.md", "docs/../../x", ".env", "img/.cache/x.png", "/etc/passwd", "a//b", "..%2Fx/"] {
            let r = requested(Some(bad));
            assert!(r.is_none() || lookup(&sample(), r.as_deref().unwrap()) == Lookup::Missing, "{bad}");
        }
        assert_eq!(requested(Some("../")), None);
        assert_eq!(requested(Some(".git/")), None);
        assert_eq!(lookup(&sample(), ".env"), Lookup::Missing);
    }

    #[test]
    fn branch_names() {
        assert!(valid_ref("main"));
        assert!(valid_ref("release/2.x"));
        assert!(!valid_ref("a:b"));
        assert!(!valid_ref("../x"));
        assert!(!valid_ref(""));
    }
}
