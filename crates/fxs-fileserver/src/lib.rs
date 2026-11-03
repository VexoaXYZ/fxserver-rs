//! External file server for FXServer's `fileserver_add`.
//!
//! The client gets every file's SHA-1 from FXServer's `getConfiguration` and
//! rejects anything else, so this server never builds content: it serves
//! FXServer's own published bytes, read from `cache/files/<res>/`:
//! - `*.rpf` sets from `cache/files/<res>/<set>`
//! - stream files by base name from `resource.sfl` (hard link or source)
//!
//! Escrowed resources (`.fxap`) are refused. FXServer runs a per-request
//! files filter for them that may block or rewrite bytes; serving them from
//! here would skip it. Keep `fileserver_add` patterns away from them too.
//!
//! On top of FXServer's handler this adds ETag / If-None-Match, single
//! `Range` requests and keep-alive HTTP/1.1 via hyper.

use axum::body::Body;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

#[derive(Clone)]
struct FileRef {
    path: PathBuf,
    hash: Option<String>,
}

#[derive(Default)]
struct ResIndex {
    sfl_mtime: Option<SystemTime>,
    stream: HashMap<String, FileRef>,
}

pub struct FileServer {
    cache_files: PathBuf,
    escrowed: RwLock<HashSet<String>>,
    index: RwLock<HashMap<String, Arc<ResIndex>>>,
    rpf_hashes: RwLock<HashMap<PathBuf, (SystemTime, u64, String)>>,
    /// Reject requests without the X-CitizenFX-Token header real clients send.
    /// This only filters casual scrapers: the token cannot be validated from
    /// outside FXServer.
    pub require_token: bool,
    pub stats: Stats,
}

#[derive(Default, serde::Serialize)]
pub struct Stats {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
    pub not_found: AtomicU64,
    pub refused_escrow: AtomicU64,
    pub not_modified: AtomicU64,
}

impl FileServer {
    pub fn new(cache_files: PathBuf) -> Self {
        Self {
            cache_files,
            escrowed: Default::default(),
            index: Default::default(),
            rpf_hashes: Default::default(),
            require_token: false,
            stats: Default::default(),
        }
    }

    /// Record which resources are escrowed, from a scan of the server root.
    pub fn set_escrowed(&self, names: impl IntoIterator<Item = String>) {
        *self.escrowed.write().unwrap() = names.into_iter().collect();
    }

    fn res_index(&self, res: &str) -> Arc<ResIndex> {
        let sfl = self.cache_files.join(res).join("resource.sfl");
        let mtime = std::fs::metadata(&sfl).and_then(|m| m.modified()).ok();
        if let Some(i) = self.index.read().unwrap().get(res) {
            if i.sfl_mtime == mtime {
                return i.clone();
            }
        }
        let mut idx = ResIndex { sfl_mtime: mtime, ..Default::default() };
        if let Ok(data) = std::fs::read(&sfl) {
            let sc = self.cache_files.join(res).join("stream_cache");
            for e in fxs_assets::sfl::decode(&data) {
                let link = sc.join(format!("z{}", e.hash));
                let path = if link.exists() { link } else { PathBuf::from(&e.on_disk_path) };
                idx.stream.entry(e.name.clone()).or_insert(FileRef { path, hash: Some(e.hash) });
            }
        }
        let idx = Arc::new(idx);
        self.index.write().unwrap().insert(res.to_string(), idx.clone());
        idx
    }

    fn rpf_hash(&self, path: &PathBuf) -> Option<String> {
        let m = std::fs::metadata(path).ok()?;
        let (mt, len) = (m.modified().ok()?, m.len());
        if let Some((a, b, h)) = self.rpf_hashes.read().unwrap().get(path) {
            if *a == mt && *b == len {
                return Some(h.clone());
            }
        }
        let (h, _) = fxs_assets::hashindex::sha1_file(path).ok()?;
        self.rpf_hashes.write().unwrap().insert(path.clone(), (mt, len, h.clone()));
        Some(h)
    }

    fn resolve(&self, res: &str, file: &str) -> Result<FileRef, StatusCode> {
        if res.is_empty() || res.contains(['/', '\\']) || res.starts_with('.') || file.contains(['/', '\\']) || file.starts_with('.') {
            return Err(StatusCode::BAD_REQUEST);
        }
        if self.escrowed.read().unwrap().contains(res) {
            self.stats.refused_escrow.fetch_add(1, Relaxed);
            return Err(StatusCode::FORBIDDEN);
        }
        if file.ends_with(".rpf") {
            let p = self.cache_files.join(res).join(file);
            if p.is_file() {
                let hash = self.rpf_hash(&p);
                return Ok(FileRef { path: p, hash });
            }
        }
        self.res_index(res).stream.get(file).cloned().ok_or(StatusCode::NOT_FOUND)
    }
}

fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        (len.saturating_sub(n), len.checked_sub(1)?)
    } else {
        let s: u64 = a.parse().ok()?;
        let e = if b.is_empty() { len.checked_sub(1)? } else { b.parse::<u64>().ok()?.min(len.checked_sub(1)?) };
        (s, e)
    };
    (start <= end && end < len).then_some((start, end))
}

async fn serve(srv: Arc<FileServer>, method: Method, headers: HeaderMap, segments: Vec<String>) -> Response {
    srv.stats.requests.fetch_add(1, Relaxed);
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if srv.require_token && headers.get("x-citizenfx-token").is_none() {
        return StatusCode::FORBIDDEN.into_response();
    }
    // Accept `/files/<res>/.../<file>` (FXServer's own shape) and
    // `/<res>/<file>` (what the client builds from a `fileserver_add` URL).
    let mut segs: Vec<&str> = segments.iter().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();
    if segs.first() == Some(&"files") && segs.len() >= 3 {
        segs.remove(0);
    }
    if segs.len() < 2 {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let res = percent_encoding::percent_decode_str(segs[0]).decode_utf8_lossy().into_owned();
    let file = percent_encoding::percent_decode_str(segs[segs.len() - 1]).decode_utf8_lossy().into_owned();

    let fr = match srv.resolve(&res, &file) {
        Ok(f) => f,
        Err(code) => {
            if code == StatusCode::NOT_FOUND {
                srv.stats.not_found.fetch_add(1, Relaxed);
            }
            return code.into_response();
        }
    };
    let Ok(mut f) = tokio::fs::File::open(&fr.path).await else {
        srv.stats.not_found.fetch_add(1, Relaxed);
        return StatusCode::NOT_FOUND.into_response();
    };
    let len = match f.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let etag = fr.hash.as_ref().map(|h| format!("\"{h}\""));
    if let (Some(et), Some(inm)) = (&etag, headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok())) {
        if inm.split(',').any(|t| t.trim() == et || t.trim() == "*") {
            srv.stats.not_modified.fetch_add(1, Relaxed);
            let mut r = StatusCode::NOT_MODIFIED.into_response();
            r.headers_mut().insert(header::ETAG, HeaderValue::from_str(et).unwrap());
            return r;
        }
    }
    let range = headers.get(header::RANGE).and_then(|v| v.to_str().ok()).and_then(|h| parse_range(h, len));
    let (status, start, count) = match range {
        Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e - s + 1),
        None => (StatusCode::OK, 0, len),
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, count)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CACHE_CONTROL, "no-cache");
    if let Some(et) = &etag {
        builder = builder.header(header::ETAG, et);
    }
    if let Some((s, e)) = range {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {s}-{e}/{len}"));
    }
    if method == Method::HEAD {
        return builder.body(Body::empty()).unwrap();
    }
    srv.stats.bytes.fetch_add(count, Relaxed);
    let body = if count <= 256 * 1024 {
        let mut buf = vec![0u8; count as usize];
        if start > 0 && f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        if f.read_exact(&mut buf).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Body::from(buf)
    } else {
        if start > 0 && f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let reader = tokio_util::io::ReaderStream::with_capacity(f.take(count), 256 * 1024);
        Body::from_stream(reader)
    };
    builder.body(body).unwrap()
}

pub fn router(srv: Arc<FileServer>) -> Router {
    Router::new()
        .route(
            "/fxs/stats",
            get(|State(s): State<Arc<FileServer>>| async move { axum::Json(serde_json::to_value(&s.stats).unwrap()) }),
        )
        .route(
            "/{*path}",
            axum::routing::any(
                |State(s): State<Arc<FileServer>>, method: Method, headers: HeaderMap, AxPath(path): AxPath<String>| async move {
                    let segs = path.split('/').map(str::to_string).collect();
                    serve(s, method, headers, segs).await
                },
            ),
        )
        .with_state(srv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn setup() -> (tempfile::TempDir, Arc<FileServer>, String) {
        let d = tempfile::tempdir().unwrap();
        let cache = d.path().join("cache/files");
        let res = cache.join("myres");
        std::fs::create_dir_all(&res).unwrap();
        std::fs::write(res.join("resource.rpf"), b"RPF2-data").unwrap();
        let src = d.path().join("prop.ydr");
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &body).unwrap();
        let (hash, _) = fxs_assets::hashindex::sha1_file(&src).unwrap();
        let e = fxs_assets::sfl::StreamEntry {
            name: "prop.ydr".into(),
            hash: hash.clone(),
            on_disk_path: src.to_string_lossy().into_owned(),
            size: body.len() as u32,
            ..Default::default()
        };
        std::fs::write(res.join("resource.sfl"), fxs_assets::sfl::encode(&[e])).unwrap();
        let srv = Arc::new(FileServer::new(cache));
        srv.set_escrowed(["paid".to_string()]);
        (d, srv, hash)
    }

    async fn get(srv: &Arc<FileServer>, uri: &str, hdr: Option<(&str, &str)>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut rb = Request::builder().uri(uri);
        if let Some((k, v)) = hdr {
            rb = rb.header(k, v);
        }
        let resp = router(srv.clone()).oneshot(rb.body(Body::empty()).unwrap()).await.unwrap();
        let (parts, body) = resp.into_parts();
        (parts.status, parts.headers, body.collect().await.unwrap().to_bytes().to_vec())
    }

    #[tokio::test]
    async fn serves_published_bytes() {
        let (_d, srv, hash) = setup();
        let (s, _, b) = get(&srv, "/files/myres/resource.rpf", None).await;
        assert_eq!((s, b.as_slice()), (StatusCode::OK, &b"RPF2-data"[..]));
        let (s, h, b) = get(&srv, "/myres/prop.ydr", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b.len(), 300_000);
        assert_eq!(h[header::ETAG], format!("\"{hash}\""));
        let (s, _, _) = get(&srv, "/myres/prop.ydr", Some(("if-none-match", &format!("\"{hash}\"")))).await;
        assert_eq!(s, StatusCode::NOT_MODIFIED);
        let (s, h, b) = get(&srv, "/myres/prop.ydr", Some(("range", "bytes=10-19"))).await;
        assert_eq!(s, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h[header::CONTENT_RANGE], "bytes 10-19/300000");
        assert_eq!(b, (10..20u32).map(|i| (i % 251) as u8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn refuses_escrow_and_traversal() {
        let (_d, srv, _) = setup();
        assert_eq!(get(&srv, "/paid/resource.rpf", None).await.0, StatusCode::FORBIDDEN);
        // lookup is by published base name only, so traversal can never resolve
        for uri in ["/myres/..%2F..%2Fsecret", "/../cache/files/myres/resource.sfl", "/myres/resource.sfl"] {
            let s = get(&srv, uri, None).await.0;
            assert!(s == StatusCode::NOT_FOUND || s == StatusCode::BAD_REQUEST, "{uri}: {s}");
        }
        assert_eq!(get(&srv, "/myres/nope.ydr", None).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn require_token() {
        let (_d, srv, _) = setup();
        let mut fs = FileServer::new(srv.cache_files.clone());
        fs.require_token = true;
        let srv = Arc::new(fs);
        assert_eq!(get(&srv, "/myres/resource.rpf", None).await.0, StatusCode::FORBIDDEN);
        assert_eq!(get(&srv, "/myres/resource.rpf", Some(("x-citizenfx-token", "abc"))).await.0, StatusCode::OK);
    }
}
