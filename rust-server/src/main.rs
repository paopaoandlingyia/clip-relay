use std::{env, net::SocketAddr, time::Duration};

use axum::body::Body;
use axum::http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use axum::{
    extract::DefaultBodyLimit,
    extract::{Multipart, Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    middleware::from_fn_with_state,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64_URL_SAFE_NO_PAD;
use base64::Engine as _;
use futures_util::{stream, Stream, StreamExt};
use mime_guess::from_path as guess_mime;
use rand::RngCore;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::convert::Infallible;
use std::fs as stdfs;
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;
use time::OffsetDateTime;

use tokio::{net::TcpListener, sync::broadcast, time as tokio_time};
use tokio_util::io::ReaderStream;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use uuid::Uuid;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;
use bytes::Bytes;
use async_trait::async_trait;

#[async_trait]
trait Storage: Send + Sync {
    async fn put(&self, path: &str, data: Vec<u8>) -> anyhow::Result<()>;
    async fn get(&self, path: &str) -> anyhow::Result<Bytes>;
    async fn delete(&self, path: &str) -> anyhow::Result<()>;
}

struct LocalStorage {
    data_dir: PathBuf,
}

#[async_trait]
impl Storage for LocalStorage {
    async fn put(&self, path: &str, data: Vec<u8>) -> anyhow::Result<()> {
        let abs = self.data_dir.join(path);
        if let Some(parent) = abs.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(abs, data).await.map_err(Into::into)
    }
    async fn get(&self, path: &str) -> anyhow::Result<Bytes> {
        let abs = self.data_dir.join(path);
        let data = tokio::fs::read(abs).await?;
        Ok(Bytes::from(data))
    }
    async fn delete(&self, path: &str) -> anyhow::Result<()> {
        let abs = self.data_dir.join(path);
        if abs.exists() {
            tokio::fs::remove_file(abs).await.map_err(Into::into)
        } else {
            Ok(())
        }
    }
}

struct S3Storage {
    client: S3Client,
    bucket: String,
    local_fallback: Arc<LocalStorage>,
}

#[async_trait]
impl Storage for S3Storage {
    async fn put(&self, path: &str, data: Vec<u8>) -> anyhow::Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(path)
            .body(ByteStream::from(data))
            .send()
            .await?;
        Ok(())
    }
    async fn get(&self, path: &str) -> anyhow::Result<Bytes> {
        // Try S3 first
        match self.client
            .get_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await
        {
            Ok(output) => {
                let data = output.body.collect().await?.into_bytes();
                Ok(data)
            }
            Err(_) => {
                // Fallback to local
                self.local_fallback.get(path).await
            }
        }
    }
    async fn delete(&self, path: &str) -> anyhow::Result<()> {
        let _ = self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(path)
            .send()
            .await;
        // Also try local
        let _ = self.local_fallback.delete(path).await;
        Ok(())
    }
}


#[derive(Clone)]
struct AppState {
    tx: broadcast::Sender<ServerEvent>,
    password: Option<String>,
    db: Arc<Mutex<Connection>>,
    data_dir: PathBuf,
    db_path: PathBuf,
    storage: Arc<dyn Storage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ServerEvent {
    #[serde(rename = "event")]
    name: String,
    data: serde_json::Value,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    // Load environment from .env in current or parent directory (best-effort)
    let _ = dotenvy::dotenv();
    if env::var("CLIPBOARD_PASSWORD").is_err() {
        let _ = dotenvy::from_filename("../.env");
    }

    let (tx, _rx) = broadcast::channel::<ServerEvent>(1024);
    let password = env::var("CLIPBOARD_PASSWORD").ok();
    let (data_dir, db_path, storage_hint) = resolve_storage_paths()?;
    tracing::info!(%storage_hint, data_dir=%data_dir.display(), db_path=%db_path.display(), "Storage initialized");
    let db = init_db(&db_path)?;

    // Storage initialization
    let s3_enabled = env::var("S3_ENDPOINT").is_ok();
    let storage: Arc<dyn Storage> = if s3_enabled {
        let endpoint = env::var("S3_ENDPOINT").unwrap();
        let bucket = env::var("S3_BUCKET").expect("S3_BUCKET must be set");
        let region = env::var("S3_REGION").unwrap_or_else(|_| "auto".into());
        
        let config = aws_config::from_env()
            .endpoint_url(endpoint)
            .region(aws_sdk_s3::config::Region::new(region))
            .load()
            .await;
        let client = aws_sdk_s3::Client::new(&config);
        
        Arc::new(S3Storage {
            client,
            bucket,
            local_fallback: Arc::new(LocalStorage {
                data_dir: data_dir.clone(),
            }),
        })
    } else {
        Arc::new(LocalStorage {
            data_dir: data_dir.clone(),
        })
    };

    let state = AppState {
        tx,
        password,
        db: Arc::new(Mutex::new(db)),
        data_dir,
        db_path,
        storage,
    };

    let protected = Router::new()
        .route("/events", get(sse_events))
        .route("/dev/broadcast", post(dev_broadcast))
        .route("/health", get(health))
        // Clipboard core
        .route("/clipboard", get(list_clipboard).post(create_clipboard))
        .route(
            "/clipboard/:id",
            get(get_clipboard).delete(delete_clipboard),
        )
        .route(
            "/clipboard/:id/share",
            get(get_item_share).put(update_item_share),
        )
        .route("/clipboard/reorder", post(reorder_clipboard))
        .route("/auth/access-token", post(issue_access_token))
        .route("/share-target", post(web_share_target))
        // Files
        .route("/files/:id", get(get_file))
        // Allow large multipart bodies (up to 210MB)
        .layer(DefaultBodyLimit::max(210 * 1024 * 1024))
        .layer(from_fn_with_state(state.clone(), auth_mw));

    let public = Router::new()
        .route("/auth/verify", post(auth_verify))
        .route("/auth/logout", post(auth_logout))
        .route("/healthz", get(health));

    let api = Router::new().nest("/api", protected.merge(public));

    // Static front-end serving (tries STATIC_DIR, then ./out, legacy ./.next-export, ../out, ../.next-export)
    let static_root = env::var("STATIC_DIR").map_or_else(
        |_| {
            let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let c1 = cwd.join("out");
            if c1.exists() {
                c1
            } else {
                let c2 = cwd.join(".next-export");
                if c2.exists() {
                    c2
                } else {
                    let p1 = cwd.join("..").join("out");
                    if p1.exists() {
                        p1
                    } else {
                        cwd.join("..").join(".next-export")
                    }
                }
            }
        },
        PathBuf::from,
    );
    // We'll handle static files ourselves to support precompressed .br
    let spa_index_path = static_root.join("index.html");
    // Share entry: prefer s/index.html, fallback to s.html (Next export may choose either)
    let share_entry_path = {
        let p1 = static_root.join("s").join("index.html");
        if p1.exists() {
            p1
        } else {
            static_root.join("s.html")
        }
    };
    let share_index_path = share_entry_path.clone();

    let app =
        Router::new()
            .merge(api)
            // Public share endpoints (no global auth) - use closures to avoid Handler type inference pitfalls
            .route("/api/share/:token", get(share_meta))
            .route("/api/share/:token/verify", post(share_verify))
            .route("/api/share/:token/qr", get(share_qr))
            .route(
                "/api/share/:token/file",
                get(
                    |State(state): State<AppState>,
                     Path(token): Path<String>,
                     headers: HeaderMap| async move {
                        share_file_inner(state, token, headers).await
                    },
                ),
            )
            .route(
                "/api/share/:token/download",
                get(
                    |State(state): State<AppState>,
                     Path(token): Path<String>,
                     headers: HeaderMap| async move {
                        share_download_inner(state, token, headers).await
                    },
                ),
            )
            // Legacy POST /api/share removed; item share managed via /api/clipboard/:id/share
            // Prefer precompressed .br for share entry as well
            .route(
                "/s",
                get({
                    let p = share_index_path.clone();
                    move |headers: HeaderMap| {
                        let p2 = p.clone();
                        async move { serve_file_prefer_br(p2, headers).await }
                    }
                }),
            )
            .route(
                "/s/",
                get({
                    let p = share_index_path.clone();
                    move |headers: HeaderMap| {
                        let p2 = p.clone();
                        async move { serve_file_prefer_br(p2, headers).await }
                    }
                }),
            )
            // Root path
            .route(
                "/",
                get({
                    let idx = spa_index_path.clone();
                    move |headers: HeaderMap| {
                        let idx2 = idx.clone();
                        async move { serve_file_prefer_br(idx2, headers).await }
                    }
                }),
            )
            // Static files with SPA fallback, prefer .br if available
            .route(
                "/*path",
                get({
                    let root = static_root.clone();
                    let idx = spa_index_path.clone();
                    move |Path(path): Path<String>, headers: HeaderMap| {
                        let root2 = root.clone();
                        let idx2 = idx.clone();
                        async move { static_handler(root2, idx2, path, headers).await }
                    }
                }),
            )
            .with_state(state)
            .layer(TraceLayer::new_for_http())
            .layer(build_cors());

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8087);
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "Rust API listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn init_tracing() {
    let filter =
        env::var("RUST_LOG").unwrap_or_else(|_| "info,tower_http=off,hyper=off".to_string());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn build_cors() -> CorsLayer {
    // 如果需要跨域并携带凭证，请通过 CORS_ALLOW_ORIGIN 指定允许的来源（逗号分隔）。
    if let Ok(origins) = env::var("CORS_ALLOW_ORIGIN") {
        let origins: Vec<HeaderValue> = origins
            .split(',')
            .filter_map(|s| HeaderValue::from_str(s.trim()).ok())
            .collect();
        CorsLayer::new()
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::DELETE,
                Method::PUT,
                Method::OPTIONS,
            ])
            .allow_headers([ACCEPT, CONTENT_TYPE, AUTHORIZATION])
            .allow_origin(origins)
            .allow_credentials(true)
    } else {
        // 同域部署不需要 CORS。为方便调试保留宽松的 Origin，但不允许携带凭证，避免与通配符冲突。
        CorsLayer::new()
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::DELETE,
                Method::PUT,
                Method::OPTIONS,
            ])
            .allow_headers([ACCEPT, CONTENT_TYPE, AUTHORIZATION])
            .allow_origin(Any)
        // 不设置 allow_credentials(true)
    }
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let verbose = env::var("HEALTH_VERBOSE")
        .ok()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    if verbose {
        Json(serde_json::json!({
            "message": "Good!",
            "dataDir": state.data_dir.display().to_string(),
            "dbPath": state.db_path.display().to_string()
        }))
    } else {
        Json(serde_json::json!({ "message": "Good!" }))
    }
}

fn accept_br(headers: &HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.split(',').any(|e| e.trim().starts_with("br")))
}

fn sanitize_path(input: &str) -> PathBuf {
    use std::path::Component;
    let mut p = PathBuf::new();
    for c in std::path::Path::new(input).components() {
        if let Component::Normal(seg) = c {
            p.push(seg);
        }
    }
    p
}

async fn serve_file_prefer_br(file_path: PathBuf, headers: HeaderMap) -> Response {
    let wants_br = accept_br(&headers);
    let orig_ct = guess_mime(&file_path).first_or_octet_stream().to_string();
    let mut chosen = file_path.clone();
    let mut ce: Option<&'static str> = None;
    if wants_br {
        let fname = chosen
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("index.html");
        let mut br_name = fname.to_string();
        br_name.push_str(".br");
        let br_path = chosen.with_file_name(br_name);
        if tokio::fs::metadata(&br_path)
            .await
            .ok()
            .is_some_and(|m| m.is_file())
        {
            chosen = br_path;
            ce = Some("br");
        }
    }
    match tokio::fs::File::open(&chosen).await {
        Ok(f) => {
            // metadata for caching
            let meta = tokio::fs::metadata(&chosen).await.ok();
            let len = meta.as_ref().map(std::fs::Metadata::len);
            let etag = meta.as_ref().and_then(make_weak_etag);
            let is_html = orig_ct.starts_with("text/html");

            // Handle conditional request via If-None-Match
            if let (Some(tag), Some(inm)) = (
                etag.as_ref(),
                headers.get(axum::http::header::IF_NONE_MATCH),
            ) {
                if inm
                    .to_str()
                    .ok()
                    .is_some_and(|s| s.split(',').any(|v| v.trim() == tag))
                {
                    let mut hm = axum::http::HeaderMap::new();
                    cache_headers_for_path(&mut hm, None, is_html);
                    if let Some(v) = ce {
                        hm.insert(
                            axum::http::header::VARY,
                            HeaderValue::from_static("Accept-Encoding"),
                        );
                        hm.insert(
                            axum::http::header::CONTENT_ENCODING,
                            HeaderValue::from_static(v),
                        );
                    }
                    hm.insert(
                        axum::http::header::ETAG,
                        HeaderValue::from_str(tag).unwrap(),
                    );
                    return (StatusCode::NOT_MODIFIED, hm).into_response();
                }
            }

            let mut hm = axum::http::HeaderMap::new();
            hm.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_str(&orig_ct).unwrap(),
            );
            if let Some(v) = ce {
                hm.insert(
                    axum::http::header::CONTENT_ENCODING,
                    HeaderValue::from_static(v),
                );
                hm.insert(
                    axum::http::header::VARY,
                    HeaderValue::from_static("Accept-Encoding"),
                );
            }
            if let Some(l) = len {
                hm.insert(
                    axum::http::header::CONTENT_LENGTH,
                    HeaderValue::from_str(&l.to_string()).unwrap(),
                );
            }
            if let Some(tag) = etag {
                hm.insert(
                    axum::http::header::ETAG,
                    HeaderValue::from_str(&tag).unwrap(),
                );
            }
            cache_headers_for_path(&mut hm, None, is_html);

            let body = Body::from_stream(ReaderStream::new(f));
            (StatusCode::OK, hm, body).into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response(),
    }
}

async fn static_handler(
    static_root: PathBuf,
    spa_index: PathBuf,
    req_path: String,
    headers: HeaderMap,
) -> Response {
    let wants_br = accept_br(&headers);
    let rel = sanitize_path(&req_path);
    let mut target = static_root.join(&rel);
    // If directory or empty => index.html
    let meta = tokio::fs::metadata(&target).await.ok();
    let target_is_file = meta.as_ref().is_some_and(|m| m.is_file());
    let target_is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
    if rel.as_os_str().is_empty() || target_is_dir {
        target = static_root.join("index.html");
    } else if !target_is_file {
        // not found -> SPA index
        target = spa_index.clone();
    }

    // Prefer .br if available
    let orig_ct = guess_mime(&target).first_or_octet_stream().to_string();
    let mut chosen = target.clone();
    let mut ce: Option<&'static str> = None;
    if wants_br {
        if let Some(fname) = chosen.file_name().and_then(|s| s.to_str()) {
            let mut br_name = fname.to_string();
            br_name.push_str(".br");
            let br_path = chosen.with_file_name(br_name);
            if tokio::fs::metadata(&br_path)
                .await
                .ok()
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                chosen = br_path;
                ce = Some("br");
            }
        }
    }

    match tokio::fs::File::open(&chosen).await {
        Ok(f) => {
            // metadata for caching
            let meta = tokio::fs::metadata(&chosen).await.ok();
            let len = meta.as_ref().map(|m| m.len());
            let etag = meta.as_ref().and_then(make_weak_etag);
            let is_html = orig_ct.starts_with("text/html");

            // conditional ETag check
            if let (Some(tag), Some(inm)) = (
                etag.as_ref(),
                headers.get(axum::http::header::IF_NONE_MATCH),
            ) {
                if inm
                    .to_str()
                    .ok()
                    .map(|s| s.split(',').any(|v| v.trim() == tag))
                    .unwrap_or(false)
                {
                    let mut hm = axum::http::HeaderMap::new();
                    cache_headers_for_path(&mut hm, Some(&req_path), is_html);
                    if let Some(v) = ce {
                        hm.insert(
                            axum::http::header::VARY,
                            HeaderValue::from_static("Accept-Encoding"),
                        );
                        hm.insert(
                            axum::http::header::CONTENT_ENCODING,
                            HeaderValue::from_static(v),
                        );
                    }
                    hm.insert(
                        axum::http::header::ETAG,
                        HeaderValue::from_str(tag).unwrap(),
                    );
                    return (StatusCode::NOT_MODIFIED, hm).into_response();
                }
            }

            let mut hm = axum::http::HeaderMap::new();
            hm.insert(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_str(&orig_ct).unwrap(),
            );
            if let Some(v) = ce {
                hm.insert(
                    axum::http::header::CONTENT_ENCODING,
                    HeaderValue::from_static(v),
                );
                hm.insert(
                    axum::http::header::VARY,
                    HeaderValue::from_static("Accept-Encoding"),
                );
            }
            if let Some(l) = len {
                hm.insert(
                    axum::http::header::CONTENT_LENGTH,
                    HeaderValue::from_str(&l.to_string()).unwrap(),
                );
            }
            if let Some(tag) = etag {
                hm.insert(
                    axum::http::header::ETAG,
                    HeaderValue::from_str(&tag).unwrap(),
                );
            }
            cache_headers_for_path(&mut hm, Some(&req_path), is_html);

            let body = Body::from_stream(ReaderStream::new(f));
            (StatusCode::OK, hm, body).into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response(),
    }
}

fn make_weak_etag(meta: &std::fs::Metadata) -> Option<String> {
    let len = meta.len();
    let mtime = meta.modified().ok()?;
    let secs = mtime.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("W/\"{}-{}\"", len, secs))
}

fn cache_headers_for_path(hm: &mut axum::http::HeaderMap, req_path: Option<&str>, is_html: bool) {
    let mut immutable = false;
    if let Some(p) = req_path {
        if p.starts_with("/_next/static/")
            || p.ends_with(".js")
            || p.ends_with(".css")
            || p.ends_with(".map")
            || p.ends_with(".json")
            || p.ends_with(".svg")
            || p.ends_with(".woff2")
            || p.ends_with(".png")
            || p.ends_with(".jpg")
            || p.ends_with(".jpeg")
            || p.ends_with(".gif")
        {
            immutable = true;
        }
    } else if !is_html {
        // For non-HTML when path unknown (e.g., "/"), prefer short cache, avoid over-caching
        hm.insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=300"),
        );
        return;
    }

    if is_html {
        hm.insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        );
    } else if immutable {
        hm.insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    } else {
        hm.insert(
            axum::http::header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=3600"),
        );
    }
}

#[derive(Deserialize)]
struct VerifyBody {
    password: String,
}

fn access_token_ttl_seconds() -> i64 {
    env::var("AUTH_TOKEN_MAX_AGE_SECONDS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(7_776_000)
}

fn issue_access_token_for(expected: &str) -> (String, i64) {
    let expires_at = OffsetDateTime::now_utc().unix_timestamp() + access_token_ttl_seconds();
    let mut buf = [0u8; 18];
    rand::thread_rng().fill_bytes(&mut buf);
    let nonce = B64_URL_SAFE_NO_PAD.encode(buf);
    let payload = format!("{expires_at}.{nonce}");
    let mut hasher = Sha256::new();
    hasher.update(expected.as_bytes());
    hasher.update(b"|access-token|");
    hasher.update(payload.as_bytes());
    let signature = B64_URL_SAFE_NO_PAD.encode(hasher.finalize());
    (format!("v1.{payload}.{signature}"), expires_at)
}

fn verify_access_token(candidate: &str, expected: &str) -> bool {
    let parts: Vec<&str> = candidate.split('.').collect();
    if parts.len() != 4 || parts[0] != "v1" {
        return false;
    }
    let expires_at = match parts[1].parse::<i64>() {
        Ok(v) => v,
        Err(_) => return false,
    };
    if expires_at <= OffsetDateTime::now_utc().unix_timestamp() {
        return false;
    }
    let payload = format!("{}.{}", parts[1], parts[2]);
    let mut hasher = Sha256::new();
    hasher.update(expected.as_bytes());
    hasher.update(b"|access-token|");
    hasher.update(payload.as_bytes());
    let signature = B64_URL_SAFE_NO_PAD.encode(hasher.finalize());
    signature == parts[3]
}

fn matches_auth_credential(candidate: &str, expected: &str) -> bool {
    candidate == expected || verify_access_token(candidate, expected)
}

async fn auth_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<VerifyBody>,
) -> Response {
    let Some(expected) = state.password.clone() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"Authentication not configured on server"})),
        )
            .into_response();
    };
    if body.password != expected {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Invalid password"})),
        )
            .into_response();
    }

    // derive scheme for cookie security
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok());
    let scheme = forwarded.unwrap_or("http").to_ascii_lowercase();
    let secure = scheme == "https";
    let samesite_env = env::var("AUTH_COOKIE_SAMESITE").unwrap_or_else(|_| "Lax".to_string());
    let samesite = match samesite_env.to_ascii_lowercase().as_str() {
        "none" => "None",
        "strict" => "Strict",
        _ => "Lax",
    };
    // Cookie 有效期：默认 7 天（可通过 AUTH_MAX_AGE_SECONDS 配置，单位：秒）
    let max_age: i64 = env::var("AUTH_MAX_AGE_SECONDS")
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(604_800); // 7 days
    let cookie = format!(
        "auth={}; Max-Age={}; Path=/; SameSite={}; HttpOnly{}",
        expected,
        max_age,
        samesite,
        if secure || samesite == "None" {
            "; Secure"
        } else {
            ""
        }
    );
    let (access_token, access_token_expires_at) = issue_access_token_for(&expected);
    let mut res = Json(serde_json::json!({
        "success": true,
        "accessToken": access_token,
        "accessTokenExpiresAt": epoch_to_iso(access_token_expires_at),
    }))
    .into_response();
    res.headers_mut()
        .insert("set-cookie", HeaderValue::from_str(&cookie).unwrap());
    res
}

async fn issue_access_token(State(state): State<AppState>) -> Response {
    let Some(expected) = state.password.as_ref() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error":"Authentication not configured on server"})),
        )
            .into_response();
    };
    let (access_token, access_token_expires_at) = issue_access_token_for(expected);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "accessToken": access_token,
            "accessTokenExpiresAt": epoch_to_iso(access_token_expires_at),
        })),
    )
        .into_response()
}

async fn auth_logout(headers: HeaderMap) -> Response {
    // 与登录时保持相同的 Cookie 属性
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok());
    let scheme = forwarded.unwrap_or("http").to_ascii_lowercase();
    let secure = scheme == "https";
    let samesite_env = env::var("AUTH_COOKIE_SAMESITE").unwrap_or_else(|_| "Lax".to_string());
    let samesite = match samesite_env.to_ascii_lowercase().as_str() {
        "none" => "None",
        "strict" => "Strict",
        _ => "Lax",
    };
    // 立刻过期
    let cookie = format!(
        "auth=; Max-Age=0; Path=/; SameSite={}; HttpOnly{}",
        samesite,
        if secure || samesite == "None" {
            "; Secure"
        } else {
            ""
        }
    );
    let mut res = Json(serde_json::json!({"success": true})).into_response();
    res.headers_mut()
        .insert("set-cookie", HeaderValue::from_str(&cookie).unwrap());
    res
}

async fn auth_mw(
    State(state): State<AppState>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    // allow unauthenticated endpoints: /api/auth/verify and public share endpoints
    let path = req.uri().path();
    if path.starts_with("/api/auth/verify") || path.starts_with("/api/healthz") {
        return next.run(req).await;
    }
    if path.starts_with("/api/share/") {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() >= 3 {
            let method = req.method().clone();
            let tail = parts.get(3).copied().unwrap_or("");
            if (parts.len() == 3 && method == Method::GET)
                || (tail == "verify" && method == Method::POST)
                || (tail == "file" && method == Method::GET)
                || (tail == "download" && method == Method::GET)
            {
                return next.run(req).await;
            }
        }
    }
    // Check password
    let expected = match state.password.as_ref() {
        Some(p) => p,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"Authentication not configured on server"})),
            )
                .into_response();
        }
    };

    // Authorization: Bearer <password> or Cookie: auth=<password>
    let headers = req.headers();
    let mut ok = false;
    if let Some(auth) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            if matches_auth_credential(token, expected.as_str()) {
                ok = true;
            }
        }
    }
    if !ok {
        if let Some(cookie) = headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
        {
            for part in cookie.split(';') {
                let part = part.trim();
                if let Some(v) = part.strip_prefix("auth=") {
                    if v == expected.as_str() {
                        ok = true;
                        break;
                    }
                }
            }
        }
    }
    if !ok {
        // Optional query-based auth: enable by setting ALLOW_QUERY_AUTH=1 (useful for SSE with cross-site cookies blocked)
        let allow_q = env::var("ALLOW_QUERY_AUTH")
            .ok()
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        if allow_q {
            if let Some(q) = req.uri().query() {
                for (k, v) in form_urlencoded::parse(q.as_bytes()) {
                    if k == "auth" && matches_auth_credential(v.as_ref(), expected.as_str()) {
                        ok = true;
                        break;
                    }
                }
            }
        }
    }
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"Unauthorized"})),
        )
            .into_response();
    }
    next.run(req).await
}

async fn sse_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.tx.subscribe();

    // stream of broadcasted events
    let broadcast_stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = serde_json::to_string(&ev.data).unwrap_or("{}".into());
                    let e = Event::default().event(ev.name).data(data);
                    yield Ok::<Event, Infallible>(e);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {},
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    // initial ready + periodic ping
    let ready =
        stream::once(async { Ok::<Event, Infallible>(Event::default().event("ready").data("{}")) });
    let ping_stream = tokio_stream::wrappers::IntervalStream::new(tokio_time::interval(
        Duration::from_millis(25_000),
    ))
    .map(|_| Ok::<Event, Infallible>(Event::default().event("ping").data("{}")));

    // Merge ping and broadcast so both can be delivered concurrently
    let merged = stream::select(ping_stream, broadcast_stream);
    let s = ready.chain(merged);

    Sse::new(s).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(25))
            .text("keep-alive"),
    )
}

#[derive(Deserialize)]
struct DevBroadcastReq {
    event: String,
    #[serde(default)]
    data: serde_json::Value,
}

async fn dev_broadcast(
    State(state): State<AppState>,
    Json(req): Json<DevBroadcastReq>,
) -> impl IntoResponse {
    let _ = state.tx.send(ServerEvent {
        name: req.event,
        data: req.data,
    });
    Json(serde_json::json!({"ok": true}))
}

// -------------------- SQLite & Models --------------------

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "UPPERCASE")]
enum ItemType {
    Text,
    Image,
    File,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ClipboardItem {
    id: String,
    #[serde(rename = "type")]
    item_type: ItemType,
    content: Option<String>,
    file_name: Option<String>,
    file_size: Option<i64>,
    sort_weight: i64,
    content_type: Option<String>,
    // backend fields
    inline_data: Option<Vec<u8>>,
    file_path: Option<String>,
    created_at: String,
    updated_at: String,
}

fn resolve_storage_paths() -> anyhow::Result<(PathBuf, PathBuf, String)> {
    // Optional override via DATA_DIR (set in Dockerfile for container deployments)
    if let Ok(data_dir) = env::var("DATA_DIR") {
        let data_dir = data_dir.trim();
        if !data_dir.is_empty() {
            let data_dir = PathBuf::from(data_dir);
            let db_path = data_dir.join("custom.db");
            ensure_writable_storage(&data_dir, Some(&db_path), Some("DATA_DIR"))?;
            return Ok((data_dir, db_path, "explicit:DATA_DIR".to_string()));
        }
    }

    // Auto-detect defaults, then fall back to known-writable locations.
    let default_data_dir = default_data_dir()?;
    let candidates: Vec<(PathBuf, &'static str)> = vec![
        (default_data_dir, "auto:default"),
        (PathBuf::from("/data"), "auto:/data"),
    ];

    let mut last_err: Option<anyhow::Error> = None;
    for (dir, hint) in candidates {
        let db_path = dir.join("custom.db");
        match ensure_writable_storage(&dir, Some(&db_path), None) {
            Ok(()) => {
                if hint != "auto:default" {
                    tracing::warn!(data_dir=%dir.display(), %hint, "Default data dir is not writable; falling back (data may be ephemeral unless you mount a volume)");
                }
                return Ok((dir, db_path, hint.to_string()));
            }
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("No writable storage directory found")))
}

fn default_data_dir() -> anyhow::Result<PathBuf> {
    // Prefer repository root's `data/` regardless of current working directory.
    // Detect repo root by finding an ancestor containing `rust-server/Cargo.toml`.
    let cwd = env::current_dir()?;
    let mut repo_root: Option<PathBuf> = None;
    for anc in cwd.ancestors() {
        let marker = anc.join("rust-server").join("Cargo.toml");
        if marker.exists() {
            repo_root = Some(anc.to_path_buf());
            break;
        }
    }

    if let Some(root) = repo_root {
        return Ok(root.join("data"));
    }

    // Container-friendly default: prefer /app if present, otherwise use cwd.
    let app_root = PathBuf::from("/app");
    let base = if app_root.exists() { app_root } else { cwd };
    Ok(base.join("data"))
}



fn ensure_writable_storage(
    data_dir: &StdPath,
    db_path: Option<&StdPath>,
    configured_by: Option<&'static str>,
) -> anyhow::Result<()> {
    stdfs::create_dir_all(data_dir.join("uploads")).map_err(|e| {
        if let Some(var) = configured_by {
            anyhow::anyhow!(
                "Failed to create storage directory (configured by {}): {} (data_dir={})",
                var,
                e,
                data_dir.display()
            )
        } else {
            anyhow::anyhow!("Failed to create storage directory: {} (data_dir={})", e, data_dir.display())
        }
    })?;

    // Verify the directory is writable (SQLite needs to create journal/WAL files too).
    let probe = data_dir.join(".write_probe");
    match std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = stdfs::remove_file(&probe);
        }
        Err(e) => {
            if let Some(var) = configured_by {
                return Err(anyhow::anyhow!(
                    "Storage directory is not writable (configured by {}): {} (data_dir={})",
                    var,
                    e,
                    data_dir.display()
                ));
            }
            return Err(anyhow::anyhow!(
                "Storage directory is not writable: {} (data_dir={})",
                e,
                data_dir.display()
            ));
        }
    }

    if let Some(db) = db_path {
        let Some(parent) = db.parent() else {
            return Err(anyhow::anyhow!("DB path must include a parent directory (db_path={})", db.display()));
        };
        // If DB lives outside data_dir, still ensure its directory is writable.
        if parent != data_dir {
            let probe = parent.join(".write_probe");
            stdfs::create_dir_all(parent).map_err(|e| {
                anyhow::anyhow!("Failed to create DB directory: {} (dir={})", e, parent.display())
            })?;
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&probe)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "DB directory is not writable: {} (dir={}, db_path={})",
                        e,
                        parent.display(),
                        db.display()
                    )
                })?;
            let _ = stdfs::remove_file(&probe);
        }
    }

    Ok(())
}

fn init_db(db_path: &StdPath) -> anyhow::Result<Connection> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch(
        r"
        PRAGMA journal_mode = WAL;
        -- Tolerate brief lock contention during concurrent writes/checkpoints.
        PRAGMA busy_timeout = 5000;
        PRAGMA foreign_keys = ON;
        CREATE TABLE IF NOT EXISTS ClipboardItem (
          id TEXT PRIMARY KEY NOT NULL,
          type TEXT NOT NULL,
          content TEXT,
          fileName TEXT,
          fileSize INTEGER,
          sortWeight INTEGER NOT NULL DEFAULT 0,
          contentType TEXT,
          inlineData BLOB,
          filePath TEXT,
          createdAt INTEGER NOT NULL DEFAULT (unixepoch()),
          updatedAt INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE INDEX IF NOT EXISTS clipboard_created_idx ON ClipboardItem (createdAt, id);

        CREATE TABLE IF NOT EXISTS ShareLink (
          token TEXT PRIMARY KEY NOT NULL,
          itemId TEXT NOT NULL,
          expiresAt INTEGER,
          maxDownloads INTEGER,
          downloadCount INTEGER NOT NULL DEFAULT 0,
          revoked INTEGER NOT NULL DEFAULT 0,
          passwordHash TEXT,
          createdAt INTEGER NOT NULL DEFAULT (unixepoch()),
          updatedAt INTEGER NOT NULL DEFAULT (unixepoch()),
          CONSTRAINT share_item_fk FOREIGN KEY (itemId) REFERENCES ClipboardItem(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS share_item_idx ON ShareLink (itemId);
        CREATE INDEX IF NOT EXISTS share_created_idx ON ShareLink (createdAt);
        ",
    )?;
    // best-effort schema migration: add passwordPlain column if missing
    let need_add_password_plain = conn
        .prepare("SELECT passwordPlain FROM ShareLink LIMIT 1")
        .map(|_| false)
        .unwrap_or(true);
    if need_add_password_plain {
        let _ = conn.execute("ALTER TABLE ShareLink ADD COLUMN passwordPlain TEXT", []);
    }
    Ok(conn)
}

fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

// -------------------- Clipboard Handlers --------------------

async fn list_clipboard(State(state): State<AppState>, uri: Uri) -> impl IntoResponse {
    let q = uri.query().unwrap_or("");
    let params: Vec<(String, String)> = form_urlencoded::parse(q.as_bytes()).into_owned().collect();
    let mut search: Option<String> = None;
    let mut take: usize = 24;
    let mut cursor_created_at: Option<i64> = None;
    let mut cursor_id: Option<String> = None;
    let mut cursor_sort: Option<i64> = None;
    for (k, v) in params {
        match k.as_str() {
            "search" => search = Some(v),
            "take" => {
                take = v
                    .parse::<usize>()
                    .ok()
                    .map(|n| n.clamp(1, 48))
                    .unwrap_or(24)
            }
            "cursorCreatedAt" => {
                // Try parse as i64 (unix ts) or RFC3339 string
                if let Ok(ts) = v.parse::<i64>() {
                    cursor_created_at = Some(ts);
                } else if let Ok(dt) =
                    time::OffsetDateTime::parse(&v, &time::format_description::well_known::Rfc3339)
                {
                    cursor_created_at = Some(dt.unix_timestamp());
                }
            }
            "cursorId" => cursor_id = Some(v),
            "cursorSortWeight" => cursor_sort = v.parse::<i64>().ok(),
            _ => {}
        }
    }
    let conn = state.db.lock().unwrap();
    let mut sql = String::from("SELECT id,type,content,fileName,fileSize,sortWeight,contentType,inlineData,filePath,createdAt,updatedAt FROM ClipboardItem");
    let mut where_clauses: Vec<String> = vec![];
    let mut params_vec: Vec<rusqlite::types::Value> = vec![];
    if let Some(s) = &search {
        where_clauses.push("(content LIKE ? OR fileName LIKE ?)".into());
        let like = format!("%{}%", s);
        params_vec.push(like.clone().into());
        params_vec.push(like.into());
    }
    if let (Some(ca), Some(cid)) = (cursor_created_at, cursor_id.as_ref()) {
        if let Some(cs) = cursor_sort {
            where_clauses.push("(sortWeight < ? OR (sortWeight = ? AND (createdAt < ? OR (createdAt = ? AND id < ?))))".into());
            params_vec.push(cs.into());
            params_vec.push(cs.into());
            params_vec.push(ca.into());
            params_vec.push(ca.into());
            params_vec.push(cid.clone().into());
        } else {
            where_clauses.push("(createdAt < ? OR (createdAt = ? AND id < ?))".into());
            params_vec.push(ca.into());
            params_vec.push(ca.into());
            params_vec.push(cid.clone().into());
        }
    }
    if !where_clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&where_clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY sortWeight DESC, createdAt DESC, id DESC LIMIT ?");
    params_vec.push(((take as i64) + 1).into());

    let mut stmt = conn.prepare(&sql).unwrap();
    let params_refs: Vec<&dyn rusqlite::ToSql> = params_vec
        .iter()
        .map(|v| v as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt
        .query_map(params_refs.as_slice(), |r| {
            Ok(ClipboardItem {
                id: r.get(0)?,
                item_type: match r.get::<_, String>(1)?.as_str() {
                    "TEXT" => ItemType::Text,
                    "IMAGE" => ItemType::Image,
                    _ => ItemType::File,
                },
                content: r.get(2).ok(),
                file_name: r.get(3).ok(),
                file_size: r.get(4).ok(),
                sort_weight: r.get(5).unwrap_or(0),
                content_type: r.get(6).ok(),
                inline_data: None,
                file_path: None,
                created_at: epoch_to_iso(r.get::<_, i64>(9).unwrap_or(0)),
                updated_at: epoch_to_iso(r.get::<_, i64>(10).unwrap_or(0)),
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let has_more = rows.len() > take;
    let items = if has_more {
        rows[..take].to_vec()
    } else {
        rows.clone()
    };
    let next_cursor = if has_more {
        Some(
            serde_json::json!({"id": items.last().unwrap().id, "createdAt": items.last().unwrap().created_at, "sortWeight": items.last().unwrap().sort_weight }),
        )
    } else {
        None
    };
    Json(serde_json::json!({"items": items, "nextCursor": next_cursor, "hasMore": has_more}))
}

fn epoch_to_iso(ts: i64) -> String {
    OffsetDateTime::from_unix_timestamp(ts)
        .unwrap_or(OffsetDateTime::now_utc())
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum InType {
    Text,
    Image,
    File,
}

const MAX_INLINE_UPLOAD: usize = 256 * 1024;

#[derive(Default)]
struct CreateClipboardPayload {
    content: Option<String>,
    in_type: Option<InType>,
    file_name: Option<String>,
    content_type: Option<String>,
    file_size: Option<i64>,
    inline_data: Option<Vec<u8>>,
    file_path_rel: Option<String>,
    share_expires_in: Option<i64>,
    share_max_downloads: Option<i64>,
    share_password: Option<String>,
}

async fn persist_upload_blob(
    state: &AppState,
    file_name: Option<&str>,
    full_data: Vec<u8>,
) -> Result<(Option<i64>, Option<Vec<u8>>, Option<String>), Response> {
    let total = full_data.len();
    if total <= MAX_INLINE_UPLOAD {
        return Ok((Some(total as i64), Some(full_data), None));
    }

    let rand_id = Uuid::new_v4().to_string();
    let ext = file_name
        .and_then(|name| StdPath::new(name).extension().and_then(|s| s.to_str()))
        .unwrap_or("");
    let generated = if ext.is_empty() {
        rand_id
    } else {
        format!("{}.{ext}", rand_id)
    };
    let rel = format!("uploads/{}", generated);
    if let Err(e) = state.storage.put(&rel, full_data).await {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "upload failed", "detail": e.to_string()})),
        )
            .into_response());
    }
    Ok((Some(total as i64), None, Some(rel)))
}

fn merge_share_target_text(title: Option<String>, text: Option<String>, url: Option<String>) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for value in [title, text, url] {
        if let Some(v) = value {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed.to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

async fn save_clipboard_item(
    state: &AppState,
    payload: CreateClipboardPayload,
) -> Result<serde_json::Value, Response> {
    let CreateClipboardPayload {
        content,
        in_type,
        file_name,
        file_size,
        content_type,
        inline_data,
        file_path_rel,
        share_expires_in,
        share_max_downloads,
        share_password,
    } = payload;

    if content.is_none() && inline_data.is_none() && file_path_rel.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"Content or file is required"})),
        )
            .into_response());
    }

    let inferred_type = in_type.unwrap_or_else(|| {
        if inline_data.is_some() || file_path_rel.is_some() {
            if content_type
                .as_deref()
                .map(|value| value.starts_with("image/"))
                .unwrap_or(false)
            {
                InType::Image
            } else {
                InType::File
            }
        } else {
            InType::Text
        }
    });

    let id = Uuid::new_v4().to_string();
    let t = match inferred_type {
        InType::Text => "TEXT",
        InType::Image => "IMAGE",
        InType::File => "FILE",
    };
    let now = now_unix();
    let new_weight: i64 = {
        let conn = state.db.lock().unwrap();
        let max: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(sortWeight),0) FROM ClipboardItem",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let w = max + 1;
        if let Err(e) = conn.execute(
            "INSERT INTO ClipboardItem (id,type,content,fileName,fileSize,sortWeight,contentType,inlineData,filePath,createdAt,updatedAt) VALUES (?,?,?,?,?,?,?, ?, ?, ?, ?)",
            params![id, t, content, file_name, file_size, w, content_type, inline_data, file_path_rel, now, now]
        ) {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"db write failed","detail": e.to_string()}))).into_response());
        }
        w
    };

    let item = serde_json::json!({
        "id": id,
        "type": t,
        "content": content,
        "fileName": file_name,
        "fileSize": file_size,
        "sortWeight": new_weight,
        "createdAt": OffsetDateTime::from_unix_timestamp(now).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        "updatedAt": OffsetDateTime::from_unix_timestamp(now).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
    });
    let _ = state.tx.send(ServerEvent {
        name: "clipboard:created".into(),
        data: item.clone(),
    });

    let (token, expires_at_abs, requires_password) = {
        let mut buf = [0u8; 18];
        rand::thread_rng().fill_bytes(&mut buf);
        let token = B64_URL_SAFE_NO_PAD.encode(buf);
        let expires_at_abs = match share_expires_in {
            Some(sec) if sec > 0 => Some(now + sec),
            _ => None,
        };
        let password_hash: Option<String> = share_password.as_ref().and_then(|p| {
            if p.trim().is_empty() {
                None
            } else {
                let mut hasher = Sha256::new();
                hasher.update(p.as_bytes());
                hasher.update(b"|");
                hasher.update(token.as_bytes());
                Some(format!("{:x}", hasher.finalize()))
            }
        });
        let password_plain: Option<String> = share_password.as_ref().and_then(|p| {
            if p.trim().is_empty() {
                None
            } else {
                Some(p.clone())
            }
        });
        {
            let conn = state.db.lock().unwrap();
            let _ = conn.execute(
                "INSERT INTO ShareLink (token,itemId,expiresAt,maxDownloads,downloadCount,revoked,passwordHash,passwordPlain,createdAt,updatedAt) VALUES (?,?,?,?,0,0,?,?,?,?)",
                params![token, id, expires_at_abs, share_max_downloads, password_hash, password_plain, now, now]
            );
        }
        (token, expires_at_abs, share_password.is_some())
    };

    let mut resp_obj = item.as_object().cloned().unwrap_or_default();
    let share_url = format!("/s/?token={}", token);
    resp_obj.insert(
        "share".to_string(),
        serde_json::json!({
            "token": token,
            "url": share_url,
            "expiresAt": expires_at_abs.map(epoch_to_iso),
            "maxDownloads": share_max_downloads,
            "requiresPassword": requires_password,
            "downloadCount": 0
        }),
    );
    Ok(serde_json::Value::Object(resp_obj))
}

async fn create_clipboard(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut payload = CreateClipboardPayload::default();
    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().map(|s| s.to_string());
        match name.as_deref() {
            Some("content") => {
                payload.content = Some(field.text().await.unwrap_or_default());
            }
            Some("type") => {
                let v = field.text().await.unwrap_or_default();
                payload.in_type = match v.as_str() {
                    "TEXT" => Some(InType::Text),
                    "IMAGE" => Some(InType::Image),
                    "FILE" => Some(InType::File),
                    _ => None,
                };
            }
            Some("file") => {
                let fname = field.file_name().map(|s| s.to_string());
                let ctype = field.content_type().map(|s| s.to_string());
                let mut full_data: Vec<u8> = Vec::new();
                let mut field_stream = field;
                while let Some(chunk) = field_stream.chunk().await.unwrap_or(None) {
                    full_data.extend_from_slice(&chunk);
                }
                let (file_size, inline_data, file_path_rel) =
                    match persist_upload_blob(&state, fname.as_deref(), full_data).await {
                        Ok(result) => result,
                        Err(resp) => return resp,
                    };
                payload.file_name = fname;
                payload.content_type = ctype;
                payload.file_size = file_size;
                payload.inline_data = inline_data;
                payload.file_path_rel = file_path_rel;
            }
            Some("shareExpiresIn") => {
                let v = field.text().await.unwrap_or_default();
                if let Ok(n) = v.parse::<i64>() {
                    payload.share_expires_in = Some(n.max(0));
                }
            }
            Some("shareMaxDownloads") => {
                let v = field.text().await.unwrap_or_default();
                if let Ok(n) = v.parse::<i64>() {
                    payload.share_max_downloads = Some(n);
                }
            }
            Some("sharePassword") => {
                let v = field.text().await.unwrap_or_default();
                if !v.trim().is_empty() {
                    payload.share_password = Some(v);
                }
            }
            _ => {}
        }
    }

    match save_clipboard_item(&state, payload).await {
        Ok(body) => (StatusCode::CREATED, Json(body)).into_response(),
        Err(resp) => resp,
    }
}

async fn web_share_target(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut title: Option<String> = None;
    let mut text_value: Option<String> = None;
    let mut url: Option<String> = None;
    let mut file_name: Option<String> = None;
    let mut content_type: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart.next_field().await.unwrap_or(None) {
        let name = field.name().map(|s| s.to_string());
        match name.as_deref() {
            Some("title") => title = Some(field.text().await.unwrap_or_default()),
            Some("text") => text_value = Some(field.text().await.unwrap_or_default()),
            Some("url") => url = Some(field.text().await.unwrap_or_default()),
            Some("files") if file_bytes.is_none() => {
                file_name = field.file_name().map(|s| s.to_string());
                content_type = field.content_type().map(|s| s.to_string());
                let mut full_data: Vec<u8> = Vec::new();
                let mut field_stream = field;
                while let Some(chunk) = field_stream.chunk().await.unwrap_or(None) {
                    full_data.extend_from_slice(&chunk);
                }
                file_bytes = Some(full_data);
            }
            _ => {}
        }
    }

    let mut payload = CreateClipboardPayload::default();
    payload.content = merge_share_target_text(title, text_value, url);

    if let Some(bytes) = file_bytes {
        let (file_size, inline_data, file_path_rel) =
            match persist_upload_blob(&state, file_name.as_deref(), bytes).await {
                Ok(result) => result,
                Err(resp) => return resp,
            };
        payload.in_type = Some(if content_type
            .as_deref()
            .map(|value| value.starts_with("image/"))
            .unwrap_or(false)
        {
            InType::Image
        } else {
            InType::File
        });
        payload.file_name = file_name;
        payload.content_type = content_type;
        payload.file_size = file_size;
        payload.inline_data = inline_data;
        payload.file_path_rel = file_path_rel;
    }

    match save_clipboard_item(&state, payload).await {
        Ok(_) => Redirect::to("/?shared=1").into_response(),
        Err(resp) => resp,
    }
}

async fn get_clipboard(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare("SELECT id,type,content,fileName,fileSize,sortWeight,contentType,inlineData,filePath,createdAt,updatedAt FROM ClipboardItem WHERE id=? LIMIT 1").unwrap();
    let row = stmt.query_row([id.clone()], |r| {
        Ok(ClipboardItem {
            id: r.get(0)?,
            item_type: match r.get::<_, String>(1)?.as_str() {
                "TEXT" => ItemType::Text,
                "IMAGE" => ItemType::Image,
                _ => ItemType::File,
            },
            content: r.get(2).ok(),
            file_name: r.get(3).ok(),
            file_size: r.get(4).ok(),
            sort_weight: r.get(5).unwrap_or(0),
            content_type: r.get(6).ok(),
            inline_data: r.get(7).ok(),
            file_path: r.get(8).ok(),
            created_at: epoch_to_iso(r.get::<_, i64>(9).unwrap_or(0)),
            updated_at: epoch_to_iso(r.get::<_, i64>(10).unwrap_or(0)),
        })
    });
    match row {
        Ok(mut item) => {
            item.inline_data = None;
            Json(item).into_response()
        }
        Err(_) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"Not found"})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ReorderReq {
    ids: Vec<String>,
}

async fn reorder_clipboard(
    State(state): State<AppState>,
    Json(req): Json<ReorderReq>,
) -> impl IntoResponse {
    if req.ids.is_empty() {
        return Json(serde_json::json!({"ok": true}));
    }
    let now = now_unix();
    let conn = state.db.lock().unwrap();
    let max: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(sortWeight),0) FROM ClipboardItem",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let base = max + (req.ids.len() as i64);
    let tx = conn.unchecked_transaction().unwrap();
    let mut weights: Vec<(String, i64)> = Vec::with_capacity(req.ids.len());
    for (i, id) in req.ids.iter().enumerate() {
        let new_weight = base - (i as i64);
        tx.execute(
            "UPDATE ClipboardItem SET sortWeight=?, updatedAt=? WHERE id=?",
            params![new_weight, now, id],
        )
        .ok();
        weights.push((id.clone(), new_weight));
    }
    tx.commit().ok();
    // Build weights mapping for SSE so clients can update local state precisely
    let mut weights_map = serde_json::Map::new();
    for (id, w) in &weights {
        weights_map.insert(id.clone(), serde_json::json!(*w));
    }
    let data = serde_json::json!({
        "ids": req.ids,
        "weights": serde_json::Value::Object(weights_map),
    });
    let _ = state.tx.send(ServerEvent {
        name: "clipboard:reordered".into(),
        data,
    });
    Json(serde_json::json!({"ok": true}))
}

async fn delete_clipboard(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let file_path: Option<String> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT filePath FROM ClipboardItem WHERE id=?")
            .unwrap();
        let fp = stmt
            .query_row([id.clone()], |r| r.get::<_, Option<String>>(0))
            .ok()
            .flatten();
        conn.execute("DELETE FROM ClipboardItem WHERE id=?", [id.clone()])
            .ok();
        fp
    };
    if let Some(rel) = file_path {
        let _ = state.storage.delete(&rel).await;
    }
    let _ = state.tx.send(ServerEvent {
        name: "clipboard:deleted".into(),
        data: serde_json::json!({"id": id}),
    });
    Json(serde_json::json!({"ok": true}))
}

type DbFileRow = (
    Option<String>,
    Option<Vec<u8>>,
    Option<String>,
    Option<String>,
);

async fn get_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
) -> impl IntoResponse {
    let q = uri.query().unwrap_or("");
    let want_download = form_urlencoded::parse(q.as_bytes())
        .into_owned()
        .any(|(k, v)| k == "download" && matches!(v.as_str(), "1" | "true" | "yes"));
    let row: Option<DbFileRow> = {
        let conn = state.db.lock().unwrap();
        let mut stmt = conn.prepare("SELECT filePath, inlineData, fileName, contentType FROM ClipboardItem WHERE id=? LIMIT 1").unwrap();
        stmt.query_row([id.clone()], |r| {
            Ok((r.get(0).ok(), r.get(1).ok(), r.get(2).ok(), r.get(3).ok()))
        })
        .ok()
    };
    if row.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"Not found"})),
        )
            .into_response();
    }
    let (file_path, inline, file_name, content_type) = row.unwrap();
    let filename = file_name.unwrap_or_else(|| "download".into());
    let ctype = content_type.unwrap_or_else(|| "application/octet-stream".into());
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_str(&ctype).unwrap(),
    );
    let disp = format!(
        "{}; filename*=UTF-8''{}",
        if want_download {
            "attachment"
        } else {
            "inline"
        },
        urlencoding::encode(&filename)
    );
    headers.insert(
        axum::http::header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disp).unwrap(),
    );
    // Strong caching: file content is immutable by id; allow long-lived cache to speed up subsequent fetches
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    if let Some(rel) = file_path {
        match state.storage.get(&rel).await {
            Ok(data) => {
                headers.insert(
                    axum::http::header::CONTENT_LENGTH,
                    HeaderValue::from_str(&data.len().to_string()).unwrap(),
                );
                let body = Body::from(data);
                (StatusCode::OK, headers, body).into_response()
            }
            Err(_) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "not found"})),
            ).into_response()
        }
    } else if let Some(data) = inline {
        headers.insert(
            axum::http::header::CONTENT_LENGTH,
            HeaderValue::from_str(&data.len().to_string()).unwrap(),
        );
        let body = Body::from(data);
        (StatusCode::OK, headers, body).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "not found"})),
        ).into_response()
    }
}


// -------------------- Share Handlers --------------------

// removed legacy share_create/share_list/share_delete/share_revoke endpoints (management moved into item share APIs)

// share_valid_row removed (no longer used)

async fn share_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> impl IntoResponse {
    // Query share row and item meta under one lock scope
    let (
        token_s,
        item_id,
        exp,
        max,
        mut dcnt,
        revoked,
        pwd_hash,
        itype,
        fname,
        fsize,
        ctype,
        content,
    ) = {
        let conn = state.db.lock().unwrap();
        let mut st = conn.prepare("SELECT token,itemId,expiresAt,maxDownloads,downloadCount,revoked,passwordHash,createdAt,updatedAt FROM ShareLink WHERE token=? LIMIT 1").unwrap();
        let row_opt = st
            .query_row([token.clone()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2).ok().flatten(),
                    r.get::<_, Option<i64>>(3).ok().flatten(),
                    r.get::<_, i64>(4).unwrap_or(0),
                    r.get::<_, i64>(5).unwrap_or(0),
                    r.get::<_, Option<String>>(6).ok().flatten(),
                    r.get::<_, i64>(7).unwrap_or(0),
                    r.get::<_, i64>(8).unwrap_or(0),
                ))
            })
            .ok();
        if row_opt.is_none() {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error":"not found"})),
            )
                .into_response();
        }
        let (token_s, item_id, exp, max, dcnt, revoked, pwd_hash, _created, _updated) =
            row_opt.unwrap();
        let (itype, fname, fsize, ctype, content): (
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT type,fileName,fileSize,contentType,content FROM ClipboardItem WHERE id=?",
                [item_id.clone()],
                |rr| {
                    Ok((
                        rr.get(0)?,
                        rr.get(1).ok(),
                        rr.get(2).ok(),
                        rr.get(3).ok(),
                        rr.get(4).ok(),
                    ))
                },
            )
            .unwrap_or(("FILE".into(), None, None, None, None));
        (
            token_s, item_id, exp, max, dcnt, revoked, pwd_hash, itype, fname, fsize, ctype,
            content,
        )
    };
    // validity check and cleanup
    let is_expired = exp.is_some_and(|e| e < now_unix());
    let is_exhausted = max.is_some_and(|m| m >= 0 && dcnt >= m);
    if revoked != 0 || is_expired || is_exhausted {
        // Delete expired/exhausted items automatically
        if is_expired || is_exhausted {
            let conn2 = state.db.lock().unwrap();
            let _ = conn2.execute("DELETE FROM ClipboardItem WHERE id=?", [item_id.clone()]);
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response();
    }
    // authorization
    let mut authorized = true;
    let needs_pwd = pwd_hash.is_some();
    if needs_pwd {
        authorized = headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .map(|c| {
                c.split(';').any(|p| {
                    p.trim() == format!("share_auth_{}={}", token_s, pwd_hash.as_ref().unwrap())
                })
            })
            .unwrap_or(false);
    }
    // If TEXT and authorized, count this access
    if authorized && itype == "TEXT" {
        let conn2 = state.db.lock().unwrap();
        let _ = conn2.execute(
            "UPDATE ShareLink SET downloadCount=downloadCount+1, updatedAt=? WHERE token=?",
            params![now_unix(), token_s.clone()],
        );
        // Keep response count in sync optimistically
        dcnt += 1;
        // Check if exhausted after increment
        if let Some(m) = max {
            if m >= 0 && dcnt >= m {
                let _ = conn2.execute("DELETE FROM ClipboardItem WHERE id=?", [item_id.clone()]);
            }
        }
    }
    Json(serde_json::json!({
        "token": token_s,
        "item": {"id": item_id, "type": itype, "fileName": fname, "fileSize": fsize, "contentType": ctype, "content": if authorized && itype=="TEXT" { content } else { None } },
        "expiresAt": exp.map(epoch_to_iso),
        "maxDownloads": max,
        "downloadCount": dcnt,
        "requiresPassword": needs_pwd,
        "authorized": authorized,
    })).into_response()
}

#[derive(Deserialize)]
struct ShareVerifyReq {
    password: String,
}

async fn share_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    Json(body): Json<ShareVerifyReq>,
) -> impl IntoResponse {
    let conn = state.db.lock().unwrap();
    let pwd_hash: Option<String> = conn
        .query_row(
            "SELECT passwordHash FROM ShareLink WHERE token=?",
            [token.clone()],
            |r| r.get(0),
        )
        .ok();
    if pwd_hash.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":"no password set"})),
        )
            .into_response();
    }
    let expected = pwd_hash.unwrap();
    let mut hasher = Sha256::new();
    hasher.update(body.password.as_bytes());
    hasher.update(b"|");
    hasher.update(token.as_bytes());
    let given = format!("{:x}", hasher.finalize());
    if given != expected {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error":"invalid password"})),
        )
            .into_response();
    }
    // cookie
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok());
    let scheme = forwarded.unwrap_or("http").to_ascii_lowercase();
    let secure = scheme == "https";
    let cookie = format!(
        "share_auth_{}={}; Max-Age=604800; Path=/; SameSite=Lax; HttpOnly{}",
        token,
        expected,
        if secure { "; Secure" } else { "" }
    );
    let mut res = Json(serde_json::json!({"success": true})).into_response();
    res.headers_mut()
        .insert("set-cookie", HeaderValue::from_str(&cookie).unwrap());
    res
}

// Return current active share for item; if none (legacy items), auto-provision a never-expiring share.
async fn get_item_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let now = now_unix();
    // Try find latest share
    let row2 = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT token, expiresAt, maxDownloads, downloadCount, revoked, passwordHash, passwordPlain, createdAt FROM ShareLink WHERE itemId=? ORDER BY createdAt DESC LIMIT 1",
            [id.clone()],
            |r| Ok((
                r.get::<_,String>(0)?, r.get::<_,Option<i64>>(1).ok().flatten(), r.get::<_,Option<i64>>(2).ok().flatten(), r.get::<_,i64>(3).unwrap_or(0), r.get::<_,i64>(4).unwrap_or(0), r.get::<_,Option<String>>(5).ok().flatten(), r.get::<_,Option<String>>(6).ok().flatten(), r.get::<_,i64>(7).unwrap_or(0)
            ))
        ).ok()
    };
    let (token, exp, max, dcnt, revoked, pwd_hash, pwd_plain) =
        if let Some((t, e, m, d, rev, ph, pp, _created)) = row2 {
            (t, e, m, d, rev, ph, pp)
        } else {
            (String::new(), None, None, 0, 0, None, None)
        };
    let mut token_out = token;
    let mut exp_out = exp;
    let mut max_out = max;
    let mut dcnt_out = dcnt;
    let mut pwd_out = pwd_hash;
    let invalid = token_out.is_empty()
        || revoked != 0
        || exp_out.map(|e| e < now).unwrap_or(false)
        || max_out.map(|m| m >= 0 && dcnt_out >= m).unwrap_or(false);
    if invalid {
        // Auto provision
        let mut buf = [0u8; 18];
        rand::thread_rng().fill_bytes(&mut buf);
        let new_token = B64_URL_SAFE_NO_PAD.encode(buf);
        let conn = state.db.lock().unwrap();
        let _ = conn.execute(
            "INSERT INTO ShareLink (token,itemId,expiresAt,maxDownloads,downloadCount,revoked,passwordHash,createdAt,updatedAt) VALUES (?,?,?,?,0,0,?,?,?)",
            params![new_token, id.clone(), Option::<i64>::None, Option::<i64>::None, Option::<String>::None, now, now]
        );
        token_out = new_token;
        exp_out = None;
        max_out = None;
        dcnt_out = 0;
        pwd_out = None;
    }
    let url = format!("/s/?token={}", token_out);
    Json(serde_json::json!({
        "token": token_out,
        "url": url,
        "expiresAt": exp_out.map(epoch_to_iso),
        "maxDownloads": max_out,
        "downloadCount": dcnt_out,
        "requiresPassword": pwd_out.is_some(),
        "password": pwd_plain,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShareUpdateReq {
    expires_in: Option<i64>,
    max_downloads: Option<i64>,
    password: Option<String>,
    reset: Option<bool>,
    disable: Option<bool>,
}

async fn update_item_share(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<ShareUpdateReq>,
) -> impl IntoResponse {
    let now = now_unix();
    let conn = state.db.lock().unwrap();
    if matches!(req.disable, Some(true)) {
        let _ = conn.execute(
            "UPDATE ShareLink SET revoked=1, updatedAt=? WHERE itemId=? AND revoked=0",
            params![now, id],
        );
        return Json(serde_json::json!({"ok": true}));
    }
    // get latest row for item or create
    type ShareSnapshot = (String, Option<i64>, Option<i64>, i64, Option<String>);
    let current: Option<ShareSnapshot> = conn.query_row(
        "SELECT token, expiresAt, maxDownloads, downloadCount, passwordHash FROM ShareLink WHERE itemId=? ORDER BY createdAt DESC LIMIT 1",
        [id.clone()], |r| Ok((r.get(0)?, r.get(1).ok().flatten(), r.get(2).ok().flatten(), r.get(3).unwrap_or(0), r.get(4).ok().flatten()))
    ).ok();
    let mut token = if let Some((t, _, _, _, _)) = current.as_ref() {
        t.clone()
    } else {
        let mut buf = [0u8; 18];
        rand::thread_rng().fill_bytes(&mut buf);
        let t = B64_URL_SAFE_NO_PAD.encode(buf);
        let _ = conn.execute("INSERT INTO ShareLink (token,itemId,expiresAt,maxDownloads,downloadCount,revoked,passwordHash,createdAt,updatedAt) VALUES (?,?,?,?,0,0,?,?,?)", params![t, id.clone(), Option::<i64>::None, Option::<i64>::None, Option::<String>::None, now, now]);
        t
    };
    let expires_abs: Option<i64> =
        req.expires_in
            .and_then(|sec| if sec > 0 { Some(now + sec) } else { None });
    let max = req
        .max_downloads
        .or_else(|| current.as_ref().and_then(|c| c.2));
    if matches!(req.reset, Some(true)) {
        let mut buf = [0u8; 18];
        rand::thread_rng().fill_bytes(&mut buf);
        let new_token = B64_URL_SAFE_NO_PAD.encode(buf);
        let new_hash: Option<String> = req.password.as_ref().and_then(|p| {
            if p.trim().is_empty() {
                None
            } else {
                let mut h = Sha256::new();
                h.update(p.as_bytes());
                h.update(b"|");
                h.update(new_token.as_bytes());
                Some(format!("{:x}", h.finalize()))
            }
        });
        let new_plain: Option<String> = req.password.as_ref().and_then(|p| {
            if p.trim().is_empty() {
                None
            } else {
                Some(p.clone())
            }
        });
        let _ = conn.execute("UPDATE ShareLink SET token=?, expiresAt=?, maxDownloads=?, passwordHash=?, passwordPlain=?, updatedAt=? WHERE token=?", params![new_token, expires_abs, max, new_hash, new_plain, now, token]);
        token = new_token;
    } else if req.password.is_some() || req.expires_in.is_some() || req.max_downloads.is_some() {
            let hash: Option<String> = req.password.as_ref().and_then(|p| {
                if p.trim().is_empty() {
                    None
                } else {
                    let mut h = Sha256::new();
                    h.update(p.as_bytes());
                    h.update(b"|");
                    h.update(token.as_bytes());
                    Some(format!("{:x}", h.finalize()))
                }
            });
            let plain: Option<String> = req.password.as_ref().and_then(|p| {
                if p.trim().is_empty() {
                    None
                } else {
                    Some(p.clone())
                }
            });
            let _ = conn.execute("UPDATE ShareLink SET expiresAt=?, maxDownloads=?, passwordHash=?, passwordPlain=?, updatedAt=? WHERE token=?", params![expires_abs, max, hash, plain, now, token]);
    }
    let (exp, maxd, dcnt, pwdhash, pwdplain):(Option<i64>, Option<i64>, i64, Option<String>, Option<String>) = conn.query_row(
        "SELECT expiresAt, maxDownloads, downloadCount, passwordHash, passwordPlain FROM ShareLink WHERE token=?",
        [token.clone()],
        |r| Ok((r.get(0).ok().flatten(), r.get(1).ok().flatten(), r.get(2).unwrap_or(0), r.get(3).ok().flatten(), r.get(4).ok().flatten()))
    ).unwrap_or((None,None,0,None,None));
    let url = format!("/s/?token={}", token);
    Json(serde_json::json!({
        "token": token,
        "url": url,
        "expiresAt": exp.map(epoch_to_iso),
        "maxDownloads": maxd,
        "downloadCount": dcnt,
        "requiresPassword": pwdhash.is_some(),
        "password": pwdplain,
    }))
}

// Generate QR code SVG for a share link. No auth required.
// GET /api/share/:token/qr?size=256&margin=2&download=0
async fn share_qr(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    uri: Uri,
) -> impl IntoResponse {
    // Validate share is still valid (not revoked/expired/exhausted)
    let row = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT expiresAt, maxDownloads, downloadCount, revoked FROM ShareLink WHERE token=?",
            [token.clone()],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0).ok().flatten(),
                    r.get::<_, Option<i64>>(1).ok().flatten(),
                    r.get::<_, i64>(2).unwrap_or(0),
                    r.get::<_, i64>(3).unwrap_or(0),
                ))
            },
        )
        .ok()
    };
    if row.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response();
    }
    let (exp, max, dcnt, revoked) = row.unwrap();
    let now = now_unix();
    let is_expired = exp.map(|e| e < now).unwrap_or(false);
    let is_exhausted = max.map(|m| m >= 0 && dcnt >= m).unwrap_or(false);
    if revoked != 0 || is_expired || is_exhausted {
        // Delete expired/exhausted items automatically
        if is_expired || is_exhausted {
            let item_id_opt: Option<String> = {
                let conn = state.db.lock().unwrap();
                conn.query_row(
                    "SELECT itemId FROM ShareLink WHERE token=?",
                    [token.clone()],
                    |r| r.get(0),
                )
                .ok()
            };
            if let Some(iid) = item_id_opt {
                let conn2 = state.db.lock().unwrap();
                let _ = conn2.execute("DELETE FROM ClipboardItem WHERE id=?", [iid]);
            }
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response();
    }

    // Parse query params
    let q = uri.query().unwrap_or("");
    let params: Vec<(String, String)> = form_urlencoded::parse(q.as_bytes()).into_owned().collect();
    let mut size: u32 = 256; // pixels
    let mut margin: u32 = 2; // modules
    let mut download: bool = false;
    for (k, v) in params {
        match k.as_str() {
            "size" => {
                size = v
                    .parse::<u32>()
                    .ok()
                    .map(|n| n.clamp(64, 4096))
                    .unwrap_or(256)
            }
            "margin" => margin = v.parse::<u32>().ok().map(|n| n.min(32)).unwrap_or(2),
            "download" => download = matches!(v.as_str(), "1" | "true" | "yes"),
            _ => {}
        }
    }

    // Build absolute URL to the share page
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("http")
        .to_ascii_lowercase();
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let url = format!("{}://{}/s/?token={}", scheme, host, token);

    // Generate QR SVG
    let svg = match make_qr_svg(&url, size, margin) {
        Ok(s) => s,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error":"qr failed"})),
            )
                .into_response()
        }
    };
    let mut hm = axum::http::HeaderMap::new();
    hm.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    hm.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=604800"),
    );
    if download {
        let fname = format!("share-{}.svg", &token[..std::cmp::min(8, token.len())]);
        hm.insert(
            axum::http::header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{fname}\""))
                .unwrap(),
        );
    }
    (StatusCode::OK, hm, svg).into_response()
}

fn make_qr_svg(text: &str, size_px: u32, margin_modules: u32) -> Result<String, ()> {
    use qrcodegen::{QrCode, QrCodeEcc};
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).map_err(|_| ())?;
    let dim = qr.size(); // modules
    let border = margin_modules as i32;
    let total_modules = dim + 2 * border;
    let scale = (size_px as f32) / (total_modules as f32);
    let mut s = String::with_capacity(1024);
    s.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    s.push_str(&format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{size_px}\" height=\"{size_px}\" viewBox=\"0 0 {size_px} {size_px}\" shape-rendering=\"crispEdges\">"));
    s.push_str("<rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>");
    // draw dark modules
    for y in 0..dim {
        for x in 0..dim {
            if qr.get_module(x, y) {
                // dark pixel
                let fx = ((x + border) as f32) * scale;
                let fy = ((y + border) as f32) * scale;
                s.push_str(&format!("<rect x=\"{fx:.3}\" y=\"{fy:.3}\" width=\"{scale:.3}\" height=\"{scale:.3}\" fill=\"#000000\"/>"));
            }
        }
    }
    s.push_str("</svg>");
    Ok(s)
}

async fn share_file_inner(state: AppState, token: String, headers: HeaderMap) -> impl IntoResponse {
    let row = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT s.token, s.passwordHash, s.maxDownloads, s.downloadCount, s.expiresAt, s.revoked, s.itemId, c.type, c.content, c.fileName, c.fileSize, c.contentType, c.filePath, c.inlineData FROM ShareLink s LEFT JOIN ClipboardItem c ON s.itemId=c.id WHERE s.token=?",
            [token.clone()],
            |r| Ok((
                r.get::<_,String>(0)?, r.get::<_,Option<String>>(1).ok().flatten(), r.get::<_,Option<i64>>(2).ok().flatten(), r.get::<_,i64>(3).unwrap_or(0),
                r.get::<_,Option<i64>>(4).ok().flatten(), r.get::<_,i64>(5).unwrap_or(0), r.get::<_,String>(6)?,
                r.get::<_,String>(7)?, r.get::<_,Option<String>>(8).ok().flatten(), r.get::<_,Option<String>>(9).ok().flatten(), r.get::<_,Option<i64>>(10).ok().flatten(),
                r.get::<_,Option<String>>(11).ok().flatten(), r.get::<_,Option<String>>(12).ok().flatten(), r.get::<_,Option<Vec<u8>>>(13).ok().flatten()
            ))
        ).ok()
    };
    if row.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response();
    }
    let (
        token_s,
        pwd_hash,
        max,
        dcnt,
        exp,
        revoked,
        item_id,
        itype,
        content,
        fname,
        _fsize,
        ctype,
        fpath,
        inline,
    ) = row.unwrap();
    // validity check and cleanup
    let is_expired = exp.is_some_and(|e| e < now_unix());
    let is_exhausted = max.is_some_and(|m| m >= 0 && dcnt >= m);
    if revoked != 0 || is_expired || is_exhausted {
        if is_expired || is_exhausted {
            let conn2 = state.db.lock().unwrap();
            let _ = conn2.execute("DELETE FROM ClipboardItem WHERE id=?", [item_id.clone()]);
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response();
    }
    // auth
    if let Some(ph) = pwd_hash.as_ref() {
        let ok = headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| {
                c.split(';')
                    .any(|p| p.trim() == format!("share_auth_{token_s}={ph}"))
            });
        if !ok {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error":"unauthorized"})),
            )
                .into_response();
        }
    }
    if itype == "TEXT" {
        let text = content.unwrap_or_default();
        // Count access for TEXT as part of meta endpoint, avoid double-count here
        return (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            )],
            text,
        )
            .into_response();
    }
    let filename = fname.unwrap_or_else(|| "download".into());
    let ctype = ctype.unwrap_or_else(|| "application/octet-stream".into());
    let mut headers_out = axum::http::HeaderMap::new();
    headers_out.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_str(&ctype).unwrap(),
    );
    headers_out.insert(
        axum::http::header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "inline; filename*=UTF-8''{}",
            urlencoding::encode(&filename)
        ))
        .unwrap(),
    );
    if let Some(rel) = fpath {
        let abs = state.data_dir.join(rel);
        if let Ok(meta) = tokio::fs::metadata(&abs).await {
            headers_out.insert(
                axum::http::header::CONTENT_LENGTH,
                HeaderValue::from_str(&meta.len().to_string()).unwrap(),
            );
        }
        match tokio::fs::File::open(&abs).await {
            Ok(f) => {
                // increment access count and check if exhausted after increment
                {
                    let conn = state.db.lock().unwrap();
                    let _= conn.execute("UPDATE ShareLink SET downloadCount=downloadCount+1, updatedAt=? WHERE token=?", params![now_unix(), token_s.clone()]);
                    if let Some(m) = max {
                        if m >= 0 && (dcnt + 1) >= m {
                            let _ = conn
                                .execute("DELETE FROM ClipboardItem WHERE id=?", [item_id.clone()]);
                        }
                    }
                }
                let stream = ReaderStream::new(f);
                let body = Body::from_stream(stream);
                return (StatusCode::OK, headers_out, body).into_response();
            }
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error":"missing"})),
                )
                    .into_response()
            }
        }
    }
    if let Some(buf) = inline {
        {
            let conn = state.db.lock().unwrap();
            let _ = conn.execute(
                "UPDATE ShareLink SET downloadCount=downloadCount+1, updatedAt=? WHERE token=?",
                params![now_unix(), token_s.clone()],
            );
            if let Some(m) = max {
                if m >= 0 && (dcnt + 1) >= m {
                    let _ = conn.execute("DELETE FROM ClipboardItem WHERE id=?", [item_id.clone()]);
                }
            }
        }
        return (StatusCode::OK, headers_out, buf).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":"missing content"})),
    )
        .into_response()
}

async fn share_download_inner(
    state: AppState,
    token: String,
    headers: HeaderMap,
) -> impl IntoResponse {
    let row = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT s.token, s.passwordHash, s.maxDownloads, s.downloadCount, s.expiresAt, s.revoked, s.itemId, c.type, c.content, c.fileName, c.fileSize, c.contentType, c.filePath, c.inlineData FROM ShareLink s LEFT JOIN ClipboardItem c ON s.itemId=c.id WHERE s.token=?",
            [token.clone()],
            |r| Ok((
                r.get::<_,String>(0)?, r.get::<_,Option<String>>(1).ok().flatten(), r.get::<_,Option<i64>>(2).ok().flatten(), r.get::<_,i64>(3).unwrap_or(0),
                r.get::<_,Option<i64>>(4).ok().flatten(), r.get::<_,i64>(5).unwrap_or(0), r.get::<_,String>(6)?,
                r.get::<_,String>(7)?, r.get::<_,Option<String>>(8).ok().flatten(), r.get::<_,Option<String>>(9).ok().flatten(), r.get::<_,Option<i64>>(10).ok().flatten(),
                r.get::<_,Option<String>>(11).ok().flatten(), r.get::<_,Option<String>>(12).ok().flatten(), r.get::<_,Option<Vec<u8>>>(13).ok().flatten()
            ))
        ).ok()
    };
    if row.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response();
    }
    let (
        token_s,
        pwd_hash,
        max,
        dcnt,
        exp,
        revoked,
        item_id,
        itype,
        content,
        fname,
        _fsize,
        ctype,
        fpath,
        inline,
    ) = row.unwrap();
    // validity check and cleanup
    let is_expired = exp.is_some_and(|e| e < now_unix());
    let is_exhausted = max.is_some_and(|m| m >= 0 && dcnt >= m);
    if revoked != 0 || is_expired || is_exhausted {
        if is_expired || is_exhausted {
            let conn2 = state.db.lock().unwrap();
            let _ = conn2.execute("DELETE FROM ClipboardItem WHERE id=?", [item_id.clone()]);
        }
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error":"not found"})),
        )
            .into_response();
    }
    if let Some(ph) = pwd_hash.as_ref() {
        let ok = headers
            .get(axum::http::header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|c| {
                c.split(';')
                    .any(|p| p.trim() == format!("share_auth_{token_s}={ph}"))
            });
        if !ok {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error":"unauthorized"})),
            )
                .into_response();
        }
    }
    // increment downloadCount and check if exhausted after increment
    {
        let conn = state.db.lock().unwrap();
        let _ = conn.execute(
            "UPDATE ShareLink SET downloadCount=downloadCount+1, updatedAt=? WHERE token=?",
            params![now_unix(), token.clone()],
        );
        if let Some(m) = max {
            if m >= 0 && (dcnt + 1) >= m {
                let _ = conn.execute("DELETE FROM ClipboardItem WHERE id=?", [item_id.clone()]);
            }
        }
    }
    if itype == "TEXT" {
        let text = content.unwrap_or_default();
        let filename = format!("{}.txt", fname.unwrap_or_else(|| "download".into()));
        return (
            StatusCode::OK,
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    HeaderValue::from_static("text/plain; charset=utf-8"),
                ),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    HeaderValue::from_str(&format!(
                        "attachment; filename*=UTF-8''{}",
                        urlencoding::encode(&filename)
                    ))
                    .unwrap(),
                ),
            ],
            text,
        )
            .into_response();
    }
    let filename = fname.unwrap_or_else(|| "download".into());
    let ctype = ctype.unwrap_or_else(|| "application/octet-stream".into());
    let mut headers_out = axum::http::HeaderMap::new();
    headers_out.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_str(&ctype).unwrap(),
    );
    headers_out.insert(
        axum::http::header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!(
            "attachment; filename*=UTF-8''{}",
            urlencoding::encode(&filename)
        ))
        .unwrap(),
    );
    if let Some(rel) = fpath {
        let abs = state.data_dir.join(rel);
        if let Ok(meta) = tokio::fs::metadata(&abs).await {
            headers_out.insert(
                axum::http::header::CONTENT_LENGTH,
                HeaderValue::from_str(&meta.len().to_string()).unwrap(),
            );
        }
        match tokio::fs::File::open(&abs).await {
            Ok(f) => {
                let stream = ReaderStream::new(f);
                let body = Body::from_stream(stream);
                return (StatusCode::OK, headers_out, body).into_response();
            }
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error":"missing"})),
                )
                    .into_response()
            }
        }
    }
    if let Some(buf) = inline {
        return (StatusCode::OK, headers_out, buf).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error":"missing content"})),
    )
        .into_response()
}

