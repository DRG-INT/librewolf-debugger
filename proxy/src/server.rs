//! The MITM proxy.
//!
//! This is the backbone layer: it is the only one that reaches every engine,
//! including Goanna (Pale Moon, Basilisk) where no extension can be installed.
//!
//! Two paths through a CONNECT:
//!   * **intercept** — terminate TLS with a minted leaf, observe, cache, record
//!   * **tunnel**    — copy bytes blind, observe nothing
//!
//! Hosts outside `only_hosts` are tunneled, never intercepted. Scoping is the
//! default posture, not an option: a trusted-CA proxy can read credentials and
//! session cookies, so it should see only what the task needs.

use crate::ca::Ca;
use crate::cache::{self, Cache};
use crate::codec;
use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use lwdbg_ssot::{Exchange, NewTrace, Store, Via};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheMode {
    /// Consult the cache, revalidate when stale, store what is storable.
    Live,
    /// Serve from cache whenever an entry exists, fresh or not. Makes repeated
    /// measurement cheap and deterministic — and is *not* representative of a
    /// real visitor, which is why every trace records its mode.
    Cached,
    /// Never read or write the cache.
    Bypass,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    /// Hosts to intercept. Empty means intercept everything, which is a wide
    /// grant — prefer naming hosts.
    pub only_hosts: Vec<String>,
    pub cache_mode: CacheMode,
    pub store_dir: PathBuf,
    pub ca_dir: PathBuf,
    /// Tor Browser's threat model is broken by a local MITM. Refused unless
    /// explicitly forced.
    pub allow_tor: bool,
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

fn full(b: impl Into<Bytes>) -> BoxBody {
    Full::new(b.into()).map_err(|e| match e {}).boxed()
}

struct Ctx {
    ca: Ca,
    cache: Mutex<Cache>,
    cfg: Config,
    client: Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        BoxBody,
    >,
    trace_id: String,
    started: Instant,
    seq: AtomicI64,
}

impl Ctx {
    /// A host is intercepted only when the scope allows it.
    fn should_intercept(&self, host: &str) -> bool {
        if self.cfg.only_hosts.is_empty() {
            return true;
        }
        let host = host.to_ascii_lowercase();
        self.cfg.only_hosts.iter().any(|h| {
            let h = h.to_ascii_lowercase();
            // A bare host matches its subdomains; `example.com` covers
            // `cdn.example.com` but must not cover `notexample.com`.
            host == h || host.ends_with(&format!(".{h}"))
        })
    }

    fn next_seq(&self) -> i64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    fn us_since_start(&self) -> i64 {
        self.started.elapsed().as_micros() as i64
    }

    fn record(&self, e: Exchange) {
        if let Ok(cache) = self.cache.lock() {
            if let Err(err) = cache.store().record_exchange(&self.trace_id, &e) {
                tracing::warn!("recording exchange failed: {err:#}");
            }
        }
    }
}

pub async fn serve(cfg: Config) -> Result<Arc<ServeHandle>> {
    // rustls needs a provider chosen explicitly. Ignore the error: a second
    // call in the same process is not a failure.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let ca = Ca::load_or_create(&cfg.ca_dir).context("loading CA")?;
    let store = Store::open(&cfg.store_dir).context("opening SSoT")?;

    let trace_id = store
        .begin_trace(&NewTrace {
            target_url: format!("proxy://{}", cfg.listen),
            host: "local".into(),
            via: Via::Proxy.as_str().into(),
            cache_mode: match cfg.cache_mode {
                CacheMode::Live => "live",
                CacheMode::Cached => "cached",
                CacheMode::Bypass => "bypass",
            }
            .into(),
            label: Some("proxy session".into()),
            ..Default::default()
        })
        .context("starting proxy trace")?;

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .build();
    let client = Client::builder(TokioExecutor::new()).build(https);

    let listener = TcpListener::bind(cfg.listen)
        .await
        .with_context(|| format!("binding {}", cfg.listen))?;
    let local_addr = listener.local_addr()?;

    let ctx = Arc::new(Ctx {
        ca,
        cache: Mutex::new(Cache::new(store)),
        cfg,
        client,
        trace_id: trace_id.clone(),
        started: Instant::now(),
        seq: AtomicI64::new(0),
    });

    let handle = Arc::new(ServeHandle {
        local_addr,
        trace_id,
        ctx: ctx.clone(),
    });

    tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("accept failed: {e}");
                    continue;
                }
            };
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req| {
                    let ctx = ctx.clone();
                    async move { entry(req, ctx).await }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .preserve_header_case(true)
                    .title_case_headers(true)
                    .serve_connection(TokioIo::new(stream), svc)
                    .with_upgrades()
                    .await
                {
                    tracing::debug!("connection ended: {e}");
                }
            });
        }
    });

    Ok(handle)
}

pub struct ServeHandle {
    pub local_addr: SocketAddr,
    pub trace_id: String,
    ctx: Arc<Ctx>,
}

impl ServeHandle {
    /// Number of exchanges recorded in this proxy session.
    pub fn exchange_count(&self) -> Result<i64> {
        let cache = self
            .ctx
            .cache
            .lock()
            .map_err(|_| anyhow!("cache mutex poisoned"))?;
        Ok(cache.store().conn().query_row(
            "SELECT COUNT(*) FROM exchange WHERE trace_id = ?1",
            rusqlite::params![self.trace_id],
            |r| r.get(0),
        )?)
    }

    pub fn ca_cert_pem(&self) -> String {
        self.ctx.ca.ca_cert_pem().to_string()
    }
}

async fn entry(req: Request<Incoming>, ctx: Arc<Ctx>) -> Result<Response<BoxBody>, hyper::Error> {
    match handle(req, ctx).await {
        Ok(r) => Ok(r),
        Err(e) => {
            tracing::warn!("request failed: {e:#}");
            let mut r = Response::new(full(format!("lwdbg proxy error: {e:#}")));
            *r.status_mut() = StatusCode::BAD_GATEWAY;
            Ok(r)
        }
    }
}

async fn handle(req: Request<Incoming>, ctx: Arc<Ctx>) -> Result<Response<BoxBody>> {
    if req.method() == Method::CONNECT {
        return handle_connect(req, ctx).await;
    }
    // Plain HTTP arrives in absolute form.
    let url = req.uri().to_string();
    forward(req, ctx, url).await
}

async fn handle_connect(req: Request<Incoming>, ctx: Arc<Ctx>) -> Result<Response<BoxBody>> {
    let authority = req
        .uri()
        .authority()
        .ok_or_else(|| anyhow!("CONNECT without authority"))?
        .clone();
    let host = authority.host().to_string();
    let port = authority.port_u16().unwrap_or(443);

    if is_tor_host(&host) && !ctx.cfg.allow_tor {
        // Proxying Tor defeats the thing it exists to do.
        let mut r = Response::new(full(
            "lwdbg refuses to intercept Tor traffic; pass --force-tor to override",
        ));
        *r.status_mut() = StatusCode::FORBIDDEN;
        return Ok(r);
    }

    let intercept = ctx.should_intercept(&host);
    let addr = format!("{host}:{port}");

    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(req).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("CONNECT upgrade failed: {e}");
                return;
            }
        };
        let io = TokioIo::new(upgraded);

        if intercept {
            if let Err(e) = intercept_tls(io, &host, port, ctx).await {
                tracing::debug!("intercept of {host} ended: {e:#}");
            }
        } else if let Err(e) = tunnel(io, &addr).await {
            tracing::debug!("tunnel to {addr} ended: {e:#}");
        }
    });

    Ok(Response::new(full(Bytes::new())))
}

/// Out-of-scope hosts are copied byte for byte and never decrypted.
async fn tunnel<I>(mut client_io: I, addr: &str) -> Result<()>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut upstream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    tokio::io::copy_bidirectional(&mut client_io, &mut upstream).await?;
    Ok(())
}

async fn intercept_tls<I>(client_io: I, host: &str, port: u16, ctx: Arc<Ctx>) -> Result<()>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let ck = ctx.ca.leaf_for(host)?;
    // PEM parsing comes from rustls-pki-types rather than rustls-pemfile:
    // the latter is unmaintained (RUSTSEC-2025-0134) and rustls moved this
    // into pki-types, which is already in the tree as a rustls dependency.
    use rustls::pki_types::pem::PemObject;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_slice_iter(ck.cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .context("parsing the minted leaf certificate")?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(ck.key_pem.as_bytes())
        .context("parsing the minted leaf private key")?;

    let mut tls_cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("building server TLS config")?;
    // Only HTTP/1.1 is served, so h2 must not be offered. Advertising h2 and
    // then speaking 1.1 makes the browser fail the connection outright.
    tls_cfg.alpn_protocols = vec![b"http/1.1".to_vec()];

    let tls = TlsAcceptor::from(Arc::new(tls_cfg))
        .accept(client_io)
        .await
        .context("TLS handshake with client")?;

    let base = if port == 443 {
        format!("https://{host}")
    } else {
        format!("https://{host}:{port}")
    };

    let svc = service_fn(move |req: Request<Incoming>| {
        let ctx = ctx.clone();
        let base = base.clone();
        async move {
            let pq = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| "/".into());
            let url = format!("{base}{pq}");
            entry_with_url(req, ctx, url).await
        }
    });

    hyper::server::conn::http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(TokioIo::new(tls), svc)
        .await?;
    Ok(())
}

async fn entry_with_url(
    req: Request<Incoming>,
    ctx: Arc<Ctx>,
    url: String,
) -> Result<Response<BoxBody>, hyper::Error> {
    match forward(req, ctx, url).await {
        Ok(r) => Ok(r),
        Err(e) => {
            tracing::warn!("forward failed: {e:#}");
            let mut r = Response::new(full(format!("lwdbg proxy error: {e:#}")));
            *r.status_mut() = StatusCode::BAD_GATEWAY;
            Ok(r)
        }
    }
}

async fn forward(req: Request<Incoming>, ctx: Arc<Ctx>, url: String) -> Result<Response<BoxBody>> {
    let seq = ctx.next_seq();
    let us_offset = ctx.us_since_start();
    let method = req.method().clone();
    let (parts, body) = req.into_parts();
    let req_headers = cache::headers_to_map(&parts.headers);

    // Cache read. The key needs the response's Vary, which we do not have on a
    // miss, so look up the no-Vary key first and re-key if the stored entry
    // declares one.
    let mut cached = None;
    if ctx.cfg.cache_mode != CacheMode::Bypass && matches!(method, Method::GET | Method::HEAD) {
        let probe = cache::derive_key(method.as_str(), &url, None, &parts.headers);
        let found = {
            let c = ctx.cache.lock().map_err(|_| anyhow!("cache poisoned"))?;
            c.lookup(&probe)?
        };
        cached = match found {
            Some(entry) => {
                let vary = entry.headers.get("vary").cloned();
                if let Some(vary) = vary.as_deref() {
                    let keyed =
                        cache::derive_key(method.as_str(), &url, Some(vary), &parts.headers);
                    let c = ctx.cache.lock().map_err(|_| anyhow!("cache poisoned"))?;
                    c.lookup(&keyed)?
                } else {
                    Some(entry)
                }
            }
            None => None,
        };
    }

    // In Cached mode a hit is served regardless of freshness: the point is a
    // deterministic, origin-free baseline.
    if let Some(c) = &cached {
        if ctx.cfg.cache_mode == CacheMode::Cached || c.fresh {
            ctx.record(Exchange {
                seq,
                us_offset,
                method: method.to_string(),
                url: url.clone(),
                status: Some(c.status as i64),
                wire_bytes: Some(c.wire_bytes),
                decoded_bytes: c.decoded_bytes,
                content_encoding: c.content_encoding.clone(),
                us_total: Some(0),
                from_cache: true,
                req_headers: serde_json::to_string(&req_headers).ok(),
                res_headers: serde_json::to_string(&c.headers).ok(),
                ..Default::default()
            });
            return build_response(c.status, &c.headers, c.body.clone(), true);
        }
    }

    // Upstream.
    let uri: Uri = url.parse().with_context(|| format!("parsing {url}"))?;
    let mut up = Request::builder().method(method.clone()).uri(uri);
    {
        let h = up.headers_mut().ok_or_else(|| anyhow!("no headers"))?;
        for (k, v) in parts.headers.iter() {
            // Hop-by-hop headers must not be forwarded.
            if is_hop_by_hop(k) {
                continue;
            }
            h.append(k.clone(), v.clone());
        }
        // Conditional revalidation of a stale entry.
        if let Some(c) = &cached {
            for (k, v) in Cache::revalidation_headers(c) {
                h.insert(k, v);
            }
        }
    }
    let up = up.body(body.boxed())?;

    let t0 = Instant::now();
    let res = ctx.client.request(up).await.context("upstream request")?;
    let us_ttfb = t0.elapsed().as_micros() as i64;
    let status = res.status();
    let res_headers = cache::headers_to_map(res.headers());
    let wire = res.into_body().collect().await?.to_bytes();
    let us_total = t0.elapsed().as_micros() as i64;

    // 304: the stale entry is still good.
    if status == StatusCode::NOT_MODIFIED {
        if let Some(c) = cached {
            ctx.record(Exchange {
                seq,
                us_offset,
                method: method.to_string(),
                url: url.clone(),
                status: Some(200),
                wire_bytes: Some(c.wire_bytes),
                decoded_bytes: c.decoded_bytes,
                content_encoding: c.content_encoding.clone(),
                us_ttfb: Some(us_ttfb),
                us_total: Some(us_total),
                from_cache: true,
                req_headers: serde_json::to_string(&req_headers).ok(),
                res_headers: serde_json::to_string(&c.headers).ok(),
                ..Default::default()
            });
            return build_response(c.status, &c.headers, c.body, true);
        }
    }

    let enc = res_headers.get("content-encoding").cloned();
    let decoded = codec::decoded_len(enc.as_deref(), &wire).map(|v| v as i64);

    let body_hash = {
        let c = ctx.cache.lock().map_err(|_| anyhow!("cache poisoned"))?;
        c.store()
            .put_blob(&wire, res_headers.get("content-type").map(|s| s.as_str()))
            .ok()
    };

    ctx.record(Exchange {
        seq,
        us_offset,
        method: method.to_string(),
        url: url.clone(),
        status: Some(status.as_u16() as i64),
        wire_bytes: Some(wire.len() as i64),
        decoded_bytes: decoded,
        content_encoding: enc,
        us_ttfb: Some(us_ttfb),
        us_total: Some(us_total),
        from_cache: false,
        req_headers: serde_json::to_string(&req_headers).ok(),
        res_headers: serde_json::to_string(&res_headers).ok(),
        body_hash,
        ..Default::default()
    });

    if ctx.cfg.cache_mode != CacheMode::Bypass
        && cache::is_storable(method.as_str(), status.as_u16(), &res_headers)
    {
        let key = cache::derive_key(
            method.as_str(),
            &url,
            res_headers.get("vary").map(|s| s.as_str()),
            &parts.headers,
        );
        let c = ctx.cache.lock().map_err(|_| anyhow!("cache poisoned"))?;
        if let Err(e) = c.put(
            &key,
            method.as_str(),
            &url,
            status.as_u16(),
            &res_headers,
            &wire,
        ) {
            tracing::warn!("cache store failed for {url}: {e:#}");
        }
        // Also store under the no-Vary probe key so the next request can find
        // the entry and learn its Vary.
        if res_headers.contains_key("vary") {
            let probe = cache::derive_key(method.as_str(), &url, None, &parts.headers);
            let _ = c.put(
                &probe,
                method.as_str(),
                &url,
                status.as_u16(),
                &res_headers,
                &wire,
            );
        }
    }

    build_response(status.as_u16(), &res_headers, wire.to_vec(), false)
}

fn build_response(
    status: u16,
    headers: &std::collections::BTreeMap<String, String>,
    body: Vec<u8>,
    from_cache: bool,
) -> Result<Response<BoxBody>> {
    let mut b = Response::builder().status(status);
    {
        let h = b.headers_mut().ok_or_else(|| anyhow!("no headers"))?;
        for (k, v) in headers {
            if is_hop_by_hop_str(k) || k == "content-length" {
                continue;
            }
            if let (Ok(name), Ok(val)) = (
                HeaderName::try_from(k.as_str()),
                HeaderValue::from_str(v.as_str()),
            ) {
                h.insert(name, val);
            }
        }
        h.insert(
            HeaderName::from_static("x-lwdbg-cache"),
            HeaderValue::from_static(if from_cache { "hit" } else { "miss" }),
        );
        h.insert(
            hyper::header::CONTENT_LENGTH,
            HeaderValue::from_str(&body.len().to_string())?,
        );
    }
    Ok(b.body(full(body))?)
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    is_hop_by_hop_str(name.as_str())
}

fn is_hop_by_hop_str(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "proxy-connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
    )
}

/// Tor's own hostnames and onion addresses.
fn is_tor_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h.ends_with(".onion") || h.ends_with("torproject.org")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_scope(hosts: &[&str]) -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            only_hosts: hosts.iter().map(|s| s.to_string()).collect(),
            cache_mode: CacheMode::Live,
            store_dir: PathBuf::from("/tmp/unused"),
            ca_dir: PathBuf::from("/tmp/unused"),
            allow_tor: false,
        }
    }

    /// `should_intercept` needs a Ctx, which needs a runtime; test the scope
    /// rule directly on the same logic.
    fn scope_matches(cfg: &Config, host: &str) -> bool {
        if cfg.only_hosts.is_empty() {
            return true;
        }
        let host = host.to_ascii_lowercase();
        cfg.only_hosts.iter().any(|h| {
            let h = h.to_ascii_lowercase();
            host == h || host.ends_with(&format!(".{h}"))
        })
    }

    #[test]
    fn empty_scope_intercepts_everything() {
        assert!(scope_matches(&ctx_with_scope(&[]), "anything.example"));
    }

    #[test]
    fn scope_covers_subdomains_but_not_lookalikes() {
        let c = ctx_with_scope(&["example.com"]);
        assert!(scope_matches(&c, "example.com"));
        assert!(scope_matches(&c, "cdn.example.com"));
        assert!(
            !scope_matches(&c, "notexample.com"),
            "suffix matching must not leak into a different registrable domain"
        );
        assert!(!scope_matches(&c, "example.com.evil.test"));
    }

    #[test]
    fn scope_is_case_insensitive() {
        let c = ctx_with_scope(&["Example.COM"]);
        assert!(scope_matches(&c, "CDN.example.com"));
    }

    #[test]
    fn tor_is_refused_by_default() {
        assert!(is_tor_host("foo.onion"));
        assert!(is_tor_host("check.torproject.org"));
        assert!(!is_tor_host("neptunium.gd"));
    }

    #[test]
    fn hop_by_hop_headers_are_not_forwarded() {
        for h in [
            "connection",
            "Proxy-Connection",
            "keep-alive",
            "Transfer-Encoding",
            "upgrade",
            "host",
        ] {
            assert!(is_hop_by_hop_str(h), "{h} should be hop-by-hop");
        }
        assert!(!is_hop_by_hop_str("content-type"));
        assert!(!is_hop_by_hop_str("content-encoding"));
    }
}
