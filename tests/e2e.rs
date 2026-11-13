//! Tests de bout en bout : le relais face à un faux GitHub (serveur axum local, adresse de l'API
//! réglée dans la config). Le faux GitHub exige le jeton sur l'API et le refuse sur les adresses
//! signées, pour vérifier qu'il n'y est jamais envoyé.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{to_bytes, Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use release_relay::config::Config;
use release_relay::{app, Relay};
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "jeton-de-test";

#[derive(Default)]
struct Fake {
    base: std::sync::OnceLock<String>,
    /// GitHub en panne : l'API répond 503
    down: AtomicBool,
    /// Appels à l'API (hors adresses signées)
    api_calls: AtomicUsize,
    /// Jeton reçu sur une adresse signée (ne doit jamais arriver)
    token_leaked: AtomicBool,
}

type FakeState = Arc<Fake>;

fn release(tag: &str) -> Value {
    let v = tag.trim_start_matches('v');
    let mut assets = vec![
        json!({ "id": 1, "name": "latest.json" }),
        json!({ "id": 2, "name": format!("App_{v}_x64-setup.exe") }),
        json!({ "id": 3, "name": format!("App_{v}_amd64.deb") }),
    ];
    if tag == "v1.0.0" {
        assets.push(json!({ "id": 99, "name": "Piege.exe" }));
    }
    json!({ "tag_name": tag, "assets": assets })
}

fn manifest() -> Value {
    let gh = "https://github.com/o/r/releases/download/v1.0.0";
    json!({
        "version": "1.0.0",
        "notes": "Nouveautés",
        "pub_date": "2026-11-13T10:00:00Z",
        "platforms": {
            "linux-x86_64-deb": { "signature": "SIG-DEB", "url": format!("{gh}/App_1.0.0_amd64.deb") },
            "windows-x86_64": { "signature": "SIG-EXE", "url": format!("{gh}/App_1.0.0_x64-setup.exe") },
            "windows-x86_64-nsis": { "signature": "SIG-EXE", "url": format!("{gh}/App_1.0.0_x64-setup.exe") }
        }
    })
}

const SHA_INDEX: &str = "1111111111111111111111111111111111111111";
const SHA_ICON: &str = "2222222222222222222222222222222222222222";
const SHA_HIDDEN: &str = "3333333333333333333333333333333333333333";
const SHA_DOCS: &str = "4444444444444444444444444444444444444444";

/// Contrôles communs de l'API : panne simulée, jeton exigé
fn api_guard(fake: &Fake, headers: &HeaderMap) -> Option<Response> {
    fake.api_calls.fetch_add(1, Ordering::SeqCst);
    if fake.down.load(Ordering::SeqCst) {
        return Some(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if auth != Some(&format!("Bearer {TOKEN}")) {
        return Some(StatusCode::UNAUTHORIZED.into_response());
    }
    None
}

async fn fake_repo(State(f): State<FakeState>, headers: HeaderMap) -> Response {
    api_guard(&f, &headers).unwrap_or_else(|| Json(json!({ "default_branch": "main" })).into_response())
}

async fn fake_latest(State(f): State<FakeState>, headers: HeaderMap) -> Response {
    api_guard(&f, &headers).unwrap_or_else(|| Json(release("v1.0.0")).into_response())
}

async fn fake_tag(State(f): State<FakeState>, Path((_, _, tag)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
    if let Some(r) = api_guard(&f, &headers) {
        return r;
    }
    match tag.as_str() {
        "v1.0.0" | "v0.9.0" => Json(release(&tag)).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn fake_asset(State(f): State<FakeState>, Path((_, _, id)): Path<(String, String, u64)>, headers: HeaderMap) -> Response {
    if let Some(r) = api_guard(&f, &headers) {
        return r;
    }
    if headers.get(header::ACCEPT).map(|v| v.as_bytes()) != Some(b"application/octet-stream") {
        return Json(json!({ "id": id })).into_response();
    }
    let location = if id == 99 { "https://ailleurs.example/vol".to_owned() } else { format!("{}/signed/{id}?sig=abc", f.base.get().unwrap()) };
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

async fn fake_signed(State(f): State<FakeState>, Path(id): Path<u64>, headers: HeaderMap) -> Response {
    if headers.contains_key(header::AUTHORIZATION) {
        f.token_leaked.store(true, Ordering::SeqCst);
        return StatusCode::BAD_REQUEST.into_response();
    }
    match id {
        1 => Json(manifest()).into_response(),
        _ => format!("installeur {id}").into_response(),
    }
}

async fn fake_contents(State(f): State<FakeState>, Path((_, _, path)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
    if let Some(r) = api_guard(&f, &headers) {
        return r;
    }
    match path.as_str() {
        "operations.json" => r#"{"operations":[]}"#.into_response(),
        "secret.txt" => "secret".into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn fake_tree(State(f): State<FakeState>, Path((_, _, treeish)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
    if let Some(r) = api_guard(&f, &headers) {
        return r;
    }
    if treeish != "v1.0.0:site" && treeish != "main:site" {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(json!({
        "sha": "x",
        "truncated": false,
        "tree": [
            { "path": "index.html", "mode": "100644", "type": "blob", "sha": SHA_INDEX },
            { "path": "icon.svg", "mode": "100644", "type": "blob", "sha": SHA_ICON },
            { "path": ".hidden", "mode": "100644", "type": "blob", "sha": SHA_HIDDEN },
            { "path": "docs", "mode": "040000", "type": "tree", "sha": "5555555555555555555555555555555555555555" },
            { "path": "docs/index.html", "mode": "100644", "type": "blob", "sha": SHA_DOCS }
        ]
    }))
    .into_response()
}

async fn fake_blob(State(f): State<FakeState>, Path((_, _, sha)): Path<(String, String, String)>, headers: HeaderMap) -> Response {
    if let Some(r) = api_guard(&f, &headers) {
        return r;
    }
    match sha.as_str() {
        SHA_INDEX => "<h1>Accueil</h1>".into_response(),
        SHA_ICON => "<svg/>".into_response(),
        SHA_HIDDEN => "caché".into_response(),
        SHA_DOCS => "<h1>Docs</h1>".into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Faux GitHub lancé sur un port libre
async fn fake_github() -> FakeState {
    let fake = FakeState::default();
    let router = Router::new()
        .route("/repos/{o}/{r}", get(fake_repo))
        .route("/repos/{o}/{r}/releases/latest", get(fake_latest))
        .route("/repos/{o}/{r}/releases/tags/{tag}", get(fake_tag))
        .route("/repos/{o}/{r}/releases/assets/{id}", get(fake_asset))
        .route("/repos/{o}/{r}/contents/{*path}", get(fake_contents))
        .route("/repos/{o}/{r}/git/trees/{treeish}", get(fake_tree))
        .route("/repos/{o}/{r}/git/blobs/{sha}", get(fake_blob))
        .route("/signed/{id}", get(fake_signed))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fake.base.set(format!("http://{}", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    fake
}

fn relay(fake: &Fake, extra: &str) -> Router {
    let base = fake.base.get().unwrap();
    let toml = format!(
        r#"
        public_url = "https://relay.test"
        rate_limit = 0
        {extra}
        [github]
        api_url = "{base}"
        download_origins = ["{base}"]

        [apps.demo]
        repo = "o/r"
        raw = ["operations.json"]
        [apps.demo.site]
        dir = "site"

        [apps.nosite]
        repo = "o/r2"
        "#
    );
    app(Relay::new(Config::from_toml(&toml).unwrap(), TOKEN).unwrap())
}

async fn call(router: &Router, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
    call_with(router, Request::get(uri).body(Body::empty()).unwrap()).await
}

async fn call_with(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    (parts.status, parts.headers, to_bytes(body, usize::MAX).await.unwrap())
}

fn location(headers: &HeaderMap) -> &str {
    headers.get(header::LOCATION).unwrap().to_str().unwrap()
}

#[tokio::test]
async fn latest_json_is_rewritten() {
    let fake = fake_github().await;
    let r = relay(&fake, "");
    let (status, headers, body) = call(&r, "/demo/latest.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let got: Value = serde_json::from_slice(&body).unwrap();
    let mut expected = manifest();
    expected["platforms"]["linux-x86_64-deb"]["url"] = json!("https://relay.test/demo/download/v1.0.0/App_1.0.0_amd64.deb");
    expected["platforms"]["windows-x86_64"]["url"] = json!("https://relay.test/demo/download/v1.0.0/App_1.0.0_x64-setup.exe");
    expected["platforms"]["windows-x86_64-nsis"]["url"] = json!("https://relay.test/demo/download/v1.0.0/App_1.0.0_x64-setup.exe");
    assert_eq!(got, expected);
    assert!(!fake.token_leaked.load(Ordering::SeqCst));
}

#[tokio::test]
async fn download_redirects_to_the_signed_url() {
    let fake = fake_github().await;
    let r = relay(&fake, "");
    let (status, headers, _) = call(&r, "/demo/download/v1.0.0/App_1.0.0_x64-setup.exe").await;
    assert_eq!(status, StatusCode::FOUND);
    let to = location(&headers).to_owned();
    assert_eq!(to, format!("{}/signed/2?sig=abc", fake.base.get().unwrap()));
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    // L'adresse signée se télécharge sans jeton
    let body = reqwest::get(&to).await.unwrap().text().await.unwrap();
    assert_eq!(body, "installeur 2");
    // Ancienne release : toujours servie
    let (status, _, _) = call(&r, "/demo/download/v0.9.0/App_0.9.0_x64-setup.exe").await;
    assert_eq!(status, StatusCode::FOUND);
}

#[tokio::test]
async fn download_refuses_unknown_files_and_hosts() {
    let fake = fake_github().await;
    let r = relay(&fake, "");
    for uri in [
        "/demo/download/v1.0.0/Autre.exe",
        "/demo/download/v1.0.0/App_0.9.0_x64-setup.exe",
        "/demo/download/v9.9.9/App_1.0.0_x64-setup.exe",
        "/demo/download/v1.0.0/..",
        "/demo/download/v1.0.0/%2E%2E%2Fsecret",
        "/inconnue/download/v1.0.0/App_1.0.0_x64-setup.exe",
    ] {
        let (status, _, _) = call(&r, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    // Redirection de l'API vers un hôte inattendu : refusée
    let (status, headers, body) = call(&r, "/demo/download/v1.0.0/Piege.exe").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(headers.get(header::LOCATION).is_none());
    assert!(!String::from_utf8_lossy(&body).contains("ailleurs"));
}

#[tokio::test]
async fn latest_platform_points_to_the_relay() {
    let fake = fake_github().await;
    let r = relay(&fake, "");
    let (status, headers, _) = call(&r, "/demo/latest/windows-x86_64").await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(location(&headers), "https://relay.test/demo/download/v1.0.0/App_1.0.0_x64-setup.exe");
    let (status, _, _) = call(&r, "/demo/latest/darwin-aarch64").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn raw_only_serves_the_whitelist() {
    let fake = fake_github().await;
    let r = relay(&fake, "");
    let (status, headers, body) = call(&r, "/demo/raw/operations.json").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json; charset=utf-8");
    assert_eq!(&body[..], br#"{"operations":[]}"#);
    let before = fake.api_calls.load(Ordering::SeqCst);
    for uri in ["/demo/raw/secret.txt", "/demo/raw/../operations.json", "/demo/raw/%2E%2E/secret.txt", "/nosite/raw/operations.json"] {
        let (status, _, _) = call(&r, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    // Refusés sans même interroger GitHub
    assert_eq!(fake.api_calls.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn presentation_page() {
    let fake = fake_github().await;
    let r = relay(&fake, "");
    let (status, headers, body) = call(&r, "/demo/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8");
    assert_eq!(&body[..], b"<h1>Accueil</h1>");
    let etag = headers[header::ETAG].clone();

    let (status, headers, _) = call(&r, "/demo/icon.svg").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/svg+xml; charset=utf-8");

    // Revalidation : 304
    let request = Request::get("/demo/").header(header::IF_NONE_MATCH, etag).body(Body::empty()).unwrap();
    let (status, _, body) = call_with(&r, request).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());

    // Dossier : index.html, après redirection vers l'adresse avec `/`
    let (status, headers, _) = call(&r, "/demo/docs").await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(location(&headers), "https://relay.test/demo/docs/");
    let (_, _, body) = call(&r, "/demo/docs/").await;
    assert_eq!(&body[..], b"<h1>Docs</h1>");
    let (status, headers, _) = call(&r, "/demo").await;
    assert_eq!(status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(location(&headers), "https://relay.test/demo/");

    // Rien en dehors du dossier, pas de fichier caché
    for uri in ["/demo/.hidden", "/demo/../operations.json", "/demo/%2E%2E/x", "/demo/absent.html", "/demo/docs/../.hidden"] {
        let (status, _, _) = call(&r, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    // Appli sans page
    for uri in ["/nosite/", "/nosite", "/nosite/index.html"] {
        let (status, _, _) = call(&r, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn page_from_the_default_branch() {
    let fake = fake_github().await;
    let base = fake.base.get().unwrap();
    let toml = format!(
        "public_url = \"https://relay.test\"\n[github]\napi_url = \"{base}\"\n[apps.b]\nrepo = \"o/r\"\n[apps.b.site]\ndir = \"site\"\nsource = \"branch\""
    );
    let r = app(Relay::new(Config::from_toml(&toml).unwrap(), TOKEN).unwrap());
    let (status, _, body) = call(&r, "/b/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"<h1>Accueil</h1>");
}

#[tokio::test]
async fn cache_and_outage() {
    let fake = fake_github().await;
    let r = relay(&fake, "cache_ttl = 1");
    assert_eq!(call(&r, "/demo/latest.json").await.0, StatusCode::OK);
    assert_eq!(call(&r, "/demo/").await.0, StatusCode::OK);
    let calls = fake.api_calls.load(Ordering::SeqCst);
    // Dans le délai de cache : aucun nouvel appel
    assert_eq!(call(&r, "/demo/latest.json").await.0, StatusCode::OK);
    assert_eq!(call(&r, "/demo/").await.0, StatusCode::OK);
    assert_eq!(fake.api_calls.load(Ordering::SeqCst), calls);
    // GitHub en panne après expiration : dernière réponse valide resservie
    fake.down.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (status, _, body) = call(&r, "/demo/latest.json").await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("https://relay.test/demo/download/v1.0.0/"));
    assert_eq!(call(&r, "/demo/").await.0, StatusCode::OK);
    assert!(fake.api_calls.load(Ordering::SeqCst) > calls);
    // Jamais vu : 502, sans détail
    let (status, _, body) = call(&r, "/demo/raw/operations.json").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(!String::from_utf8_lossy(&body).contains(TOKEN));
}

#[tokio::test]
async fn token_never_in_responses() {
    let fake = fake_github().await;
    let r = relay(&fake, "");
    fake.down.store(true, Ordering::SeqCst);
    for uri in ["/demo/latest.json", "/demo/download/v1.0.0/App_1.0.0_x64-setup.exe", "/demo/", "/demo/raw/operations.json"] {
        let (_, headers, body) = call(&r, uri).await;
        assert!(!String::from_utf8_lossy(&body).contains(TOKEN), "{uri}");
        assert!(headers.values().all(|v| !String::from_utf8_lossy(v.as_bytes()).contains(TOKEN)), "{uri}");
    }
}

#[tokio::test]
async fn routing_basics() {
    let fake = fake_github().await;
    let r = relay(&fake, "");
    assert_eq!(call(&r, "/health").await.0, StatusCode::OK);
    assert_eq!(call(&r, "/").await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&r, "/inconnue/latest.json").await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&r, "/Demo/latest.json").await.0, StatusCode::NOT_FOUND);
    let post = Request::post("/demo/latest.json").body(Body::empty()).unwrap();
    assert_eq!(call_with(&r, post).await.0, StatusCode::METHOD_NOT_ALLOWED);
    let (_, headers, _) = call(&r, "/health").await;
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
}

#[tokio::test]
async fn rate_limited_per_address() {
    let fake = fake_github().await;
    let base = fake.base.get().unwrap();
    let toml = format!("public_url = \"https://relay.test\"\nrate_limit = 2\n[github]\napi_url = \"{base}\"\n[apps.demo]\nrepo = \"o/r\"");
    let r = app(Relay::new(Config::from_toml(&toml).unwrap(), TOKEN).unwrap());
    let from = |ip: &str| Request::get("/demo/raw/x").header("x-forwarded-for", ip).body(Body::empty()).unwrap();
    assert_eq!(call_with(&r, from("203.0.113.1")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call_with(&r, from("203.0.113.1")).await.0, StatusCode::NOT_FOUND);
    let (status, headers, _) = call_with(&r, from("203.0.113.1")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(headers.contains_key(header::RETRY_AFTER));
    assert_eq!(call_with(&r, from("203.0.113.2")).await.0, StatusCode::NOT_FOUND);
    // /health n'est pas limité
    for _ in 0..5 {
        assert_eq!(call(&r, "/health").await.0, StatusCode::OK);
    }
}
