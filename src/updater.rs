//! Mises à jour : `latest.json` réécrit, téléchargement des assets par redirection et
//! installeur de la dernière version.
//!
//! Le manifeste est celui publié dans la release (asset `latest.json`) : seules les adresses
//! `platforms.*.url` changent, les signatures restent celles de la release. L'installeur ne
//! transite jamais par le serveur : le client est redirigé vers l'adresse signée de GitHub.

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use percent_encoding::percent_decode_str;
use serde::Deserialize;
use serde_json::Value;

use crate::config::App;
use crate::github::ACCEPT_OCTET;
use crate::paths::{self, encode};
use crate::{RelayError, Shared};

/// Nom de l'asset manifeste dans chaque release
const MANIFEST: &str = "latest.json";

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub id: u64,
    pub name: String,
}

impl Release {
    fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

/// Dernière release publiée (ni brouillon, ni préversion)
pub async fn latest_release(relay: &Shared, app: &App) -> Result<Release, RelayError> {
    let release: Release = relay.github.get_json(&format!("/repos/{}/{}/releases/latest", app.owner, app.repo)).await?;
    if !paths::valid_name(&release.tag_name) {
        return Err(RelayError::Upstream(format!("{}/{} : tag inattendu {:?}", app.owner, app.repo, release.tag_name)));
    }
    Ok(release)
}

async fn release_by_tag(relay: &Shared, app: &App, tag: &str) -> Result<Release, RelayError> {
    Ok(relay.github.get_json(&format!("/repos/{}/{}/releases/tags/{}", app.owner, app.repo, encode(tag))).await?)
}

/// Manifeste publié dans la release
async fn release_manifest(relay: &Shared, app: &App, release: &Release) -> Result<Value, RelayError> {
    let asset = release.asset(MANIFEST).ok_or(RelayError::NotFound)?;
    let body = relay.github.asset(&app.owner, &app.repo, asset.id).await?;
    serde_json::from_slice(&body)
        .map_err(|e| RelayError::Upstream(format!("{}/{} {} : {MANIFEST} illisible ({e})", app.owner, app.repo, release.tag_name)))
}

/// Nom de fichier d'une adresse du manifeste (dernier segment, décodé)
fn file_name(url: &str) -> Option<String> {
    let url = reqwest::Url::parse(url).ok()?;
    let last = url.path_segments()?.next_back()?;
    let name = percent_decode_str(last).decode_utf8().ok()?.into_owned();
    paths::valid_name(&name).then_some(name)
}

/// Fichier d'une plateforme du manifeste, vérifié parmi les assets de la release
fn platform_file(manifest: &Value, key: &str, release: &Release) -> Result<String, String> {
    let url = manifest["platforms"][key]["url"].as_str().ok_or_else(|| format!("plateforme {key} sans url"))?;
    let name = file_name(url).ok_or_else(|| format!("plateforme {key} : adresse inattendue"))?;
    if release.asset(&name).is_none() {
        return Err(format!("plateforme {key} : {name} n'est pas un asset de {}", release.tag_name));
    }
    Ok(name)
}

/// Adresses `platforms.*.url` réécrites en `{base}/download/{tag}/{fichier}` ; le reste du
/// manifeste (version, notes, signatures…) est gardé tel quel
pub fn rewrite(manifest: &mut Value, base: &str, release: &Release) -> Result<(), String> {
    let keys: Vec<String> = manifest
        .get("platforms")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("{MANIFEST} sans platforms"))?
        .keys()
        .cloned()
        .collect();
    for key in keys {
        let name = platform_file(manifest, &key, release)?;
        let url = format!("{base}/download/{}/{}", encode(&release.tag_name), encode(&name));
        manifest["platforms"][&key]["url"] = Value::String(url);
    }
    Ok(())
}

/// Adresse publique d'une appli (`{public_url}/{app}`)
fn app_base(relay: &Shared, name: &str) -> String {
    format!("{}/{name}", relay.config.public_url)
}

fn redirect(location: &str, cache: &'static str) -> Result<Response, RelayError> {
    let location = HeaderValue::from_str(location).map_err(|_| RelayError::Upstream("adresse de redirection invalide".into()))?;
    Ok((StatusCode::FOUND, [(header::LOCATION, location), (header::CACHE_CONTROL, HeaderValue::from_static(cache))]).into_response())
}

/// `GET /{app}/latest.json`
pub async fn manifest(State(relay): State<Shared>, Path(name): Path<String>) -> Result<Response, RelayError> {
    let app = relay.app(&name)?;
    let release = latest_release(&relay, app).await?;
    let mut manifest = release_manifest(&relay, app, &release).await?;
    rewrite(&mut manifest, &app_base(&relay, &name), &release)
        .map_err(|e| RelayError::Upstream(format!("{}/{} {} : {e}", app.owner, app.repo, release.tag_name)))?;
    let body = serde_json::to_vec_pretty(&manifest).map_err(|e| RelayError::Upstream(e.to_string()))?;
    let cache = format!("public, max-age={}", relay.config.cache_ttl.as_secs());
    Ok((
        [(header::CONTENT_TYPE, HeaderValue::from_static("application/json")), (header::CACHE_CONTROL, HeaderValue::from_str(&cache).unwrap())],
        body,
    )
        .into_response())
}

/// `GET /{app}/download/{tag}/{fichier}` : le fichier doit être un asset de cette release ;
/// 302 vers l'adresse signée donnée par l'API (origine vérifiée)
pub async fn download(State(relay): State<Shared>, Path((name, tag, file)): Path<(String, String, String)>) -> Result<Response, RelayError> {
    let app = relay.app(&name)?;
    if !paths::valid_name(&tag) || !paths::valid_name(&file) {
        return Err(RelayError::NotFound);
    }
    let release = release_by_tag(&relay, app, &tag).await?;
    let asset = release.asset(&file).ok_or(RelayError::NotFound)?;
    let path = format!("/repos/{}/{}/releases/assets/{}", app.owner, app.repo, asset.id);
    let location = relay.github.redirect_location(&path, ACCEPT_OCTET).await?;
    redirect(location.as_str(), "no-store")
}

/// `GET /{app}/latest/{plateforme}` : installeur de la dernière version pour une clé de
/// `latest.json`, par le relais (`/{app}/download/…`)
pub async fn latest(State(relay): State<Shared>, Path((name, platform)): Path<(String, String)>) -> Result<Response, RelayError> {
    let app = relay.app(&name)?;
    if !paths::valid_name(&platform) {
        return Err(RelayError::NotFound);
    }
    let release = latest_release(&relay, app).await?;
    let manifest = release_manifest(&relay, app, &release).await?;
    if manifest["platforms"].get(&platform).is_none() {
        return Err(RelayError::NotFound);
    }
    let file = platform_file(&manifest, &platform, &release)
        .map_err(|e| RelayError::Upstream(format!("{}/{} {} : {e}", app.owner, app.repo, release.tag_name)))?;
    let location = format!("{}/download/{}/{}", app_base(&relay, &name), encode(&release.tag_name), encode(&file));
    redirect(&location, "no-cache")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release() -> Release {
        let names = ["latest.json", "App_1.2.3_amd64.AppImage", "App_1.2.3_amd64.deb", "App_1.2.3_x64-setup.exe"];
        Release {
            tag_name: "v1.2.3".into(),
            assets: names.iter().enumerate().map(|(i, n)| Asset { id: i as u64 + 1, name: n.to_string() }).collect(),
        }
    }

    fn manifest() -> Value {
        let gh = "https://github.com/o/r/releases/download/v1.2.3";
        json!({
            "version": "1.2.3",
            "notes": "Notes",
            "pub_date": "2026-11-13T10:00:00Z",
            "platforms": {
                "linux-x86_64": { "signature": "sigA", "url": format!("{gh}/App_1.2.3_amd64.AppImage") },
                "linux-x86_64-appimage": { "signature": "sigA", "url": format!("{gh}/App_1.2.3_amd64.AppImage") },
                "linux-x86_64-deb": { "signature": "sigD", "url": format!("{gh}/App_1.2.3_amd64.deb") },
                "windows-x86_64": { "signature": "sigW", "url": format!("{gh}/App_1.2.3_x64-setup.exe") },
                "windows-x86_64-nsis": { "signature": "sigW", "url": format!("{gh}/App_1.2.3_x64-setup.exe") }
            }
        })
    }

    #[test]
    fn rewrites_only_the_urls() {
        let mut m = manifest();
        rewrite(&mut m, "https://relay.fr/app", &release()).unwrap();
        let p = &m["platforms"];
        assert_eq!(p["windows-x86_64"]["url"], "https://relay.fr/app/download/v1.2.3/App_1.2.3_x64-setup.exe");
        assert_eq!(p["windows-x86_64-nsis"]["url"], "https://relay.fr/app/download/v1.2.3/App_1.2.3_x64-setup.exe");
        assert_eq!(p["linux-x86_64-deb"]["url"], "https://relay.fr/app/download/v1.2.3/App_1.2.3_amd64.deb");
        assert_eq!(p["linux-x86_64"]["url"], "https://relay.fr/app/download/v1.2.3/App_1.2.3_amd64.AppImage");
        // Tout le reste est inchangé
        let mut expected = manifest();
        for (k, v) in expected["platforms"].as_object_mut().unwrap() {
            v["url"] = p[k]["url"].clone();
        }
        assert_eq!(m, expected);
        // Ordre des clés gardé
        assert_eq!(m.as_object().unwrap().keys().collect::<Vec<_>>(), ["version", "notes", "pub_date", "platforms"]);
    }

    #[test]
    fn refuses_files_outside_the_release() {
        let mut m = manifest();
        m["platforms"]["windows-x86_64"]["url"] = json!("https://github.com/o/r/releases/download/v1.2.3/Autre.exe");
        assert!(rewrite(&mut m, "https://relay.fr/app", &release()).is_err());
        let mut m = manifest();
        m["platforms"]["windows-x86_64"]["url"] = json!("https://github.com/o/r/releases/download/v1.2.3/..");
        assert!(rewrite(&mut m, "https://relay.fr/app", &release()).is_err());
        let mut m = json!({ "version": "1.2.3" });
        assert!(rewrite(&mut m, "https://relay.fr/app", &release()).is_err());
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name("https://github.com/o/r/releases/download/v1/App_1.0_x64-setup.exe").as_deref(), Some("App_1.0_x64-setup.exe"));
        assert_eq!(file_name("https://h/a/App%2B1.exe").as_deref(), Some("App+1.exe"));
        assert_eq!(file_name("https://h/a/%2E%2E"), None);
        assert_eq!(file_name("https://h/"), None);
        assert_eq!(file_name("pas une adresse"), None);
    }

    #[test]
    fn asset_selection() {
        let r = release();
        assert_eq!(r.asset("App_1.2.3_amd64.deb").map(|a| a.id), Some(3));
        assert!(r.asset("app_1.2.3_amd64.deb").is_none());
        assert!(r.asset("App_1.2.3_amd64.deb.sig").is_none());
    }
}
