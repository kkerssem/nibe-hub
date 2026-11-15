//! Read-only MCP server on top of the NIBE collector's SQLite history.

mod tools;

use anyhow::{Context, Result};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use rmcp::transport::streamable_http_server::{session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService};
use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream},
    sync::Arc,
    time::Duration,
};
use nibe_core::config::env;
use tools::NibeServer;
use tracing::{info, warn};

#[derive(Clone)]
struct Auth {
    token: Option<Arc<String>>,
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_token(State(auth): State<Auth>, req: Request, next: Next) -> Response {
    if let Some(token) = &auth.token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|t| ct_eq(t.trim().as_bytes(), token.as_bytes()))
            .unwrap_or(false);
        if !ok {
            return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "unauthorized").into_response();
        }
    }
    next.run(req).await
}

/// Accepts any server certificate. Only for `nibe-mcp health`, which talks to its own server on
/// localhost and checks that it answers, not who it is.
#[derive(Debug)]
struct AnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Sends `GET /healthz` to `target` (over TLS when `tls`) and returns the response.
fn get_healthz(target: SocketAddr, tls: bool) -> Result<String> {
    const TIMEOUT: Duration = Duration::from_secs(3);
    let tcp = TcpStream::connect_timeout(&target, TIMEOUT).context("connecting")?;
    tcp.set_read_timeout(Some(TIMEOUT))?;
    tcp.set_write_timeout(Some(TIMEOUT))?;
    let request = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let mut response = Vec::new();
    // A read error after the response arrived (a TLS peer that closes without close_notify)
    // doesn't matter: only what was received counts.
    let read_result = if tls {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyServerCert(provider)))
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::IpAddress(target.ip().into());
        let conn = rustls::ClientConnection::new(Arc::new(config), name)?;
        let mut stream = rustls::StreamOwned::new(conn, tcp);
        stream.write_all(request).context("sending the request over TLS")?;
        stream.read_to_end(&mut response)
    } else {
        let mut stream = tcp;
        stream.write_all(request).context("sending the request")?;
        stream.read_to_end(&mut response)
    };
    if response.is_empty() {
        read_result.context("reading the response")?;
    }
    Ok(String::from_utf8_lossy(&response).into_owned())
}

/// `nibe-mcp health`, for a container health check: exit code 0 when `GET /healthz` on the
/// port in MCP_BIND answers `200 ok`, over HTTPS when MCP_TLS_CERT is set (the certificate is
/// not checked: only whether the server answers matters). For a 0.0.0.0 or [::] bind it asks
/// localhost, trying 127.0.0.1 too for [::]. Needs neither the token nor the certificate.
fn health() -> i32 {
    let bind: SocketAddr = match env("MCP_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()).parse() {
        Ok(b) => b,
        Err(e) => {
            println!("unhealthy: MCP_BIND: {e}");
            return 1;
        }
    };
    let port = bind.port();
    let targets: Vec<SocketAddr> = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => vec![(Ipv4Addr::LOCALHOST, port).into()],
        IpAddr::V6(ip) if ip.is_unspecified() => vec![(Ipv6Addr::LOCALHOST, port).into(), (Ipv4Addr::LOCALHOST, port).into()],
        _ => vec![bind],
    };
    let tls = env("MCP_TLS_CERT").is_some();
    let scheme = if tls { "https" } else { "http" };
    let mut problems = Vec::new();
    for target in targets {
        match get_healthz(target, tls) {
            Ok(r) if r.starts_with("HTTP/1.1 200") && r.trim_end().ends_with("ok") => {
                println!("healthy: {scheme}://{target}/healthz answers ok");
                return 0;
            }
            Ok(r) => problems.push(format!("{target}: unexpected answer {:?}", r.lines().next().unwrap_or(""))),
            Err(e) => problems.push(format!("{target}: {e:#}")),
        }
    }
    println!("unhealthy: {scheme} /healthz: {}", problems.join("; "));
    1
}

#[tokio::main]
async fn main() -> Result<()> {
    if nibe_core::config::health_requested() {
        std::process::exit(health());
    }
    nibe_core::logging::init();

    let db_path = nibe_core::config::db_path();
    let bind: SocketAddr = env("MCP_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()).parse().context("MCP_BIND")?;
    let tz = nibe_core::timeutil::parse_tz(&env("TZ_NAME").unwrap_or_else(|| "Europe/Amsterdam".into()))?;
    let token = env("MCP_TOKEN");
    let allow_no_auth = env("MCP_ALLOW_NO_AUTH").map(|v| v == "true" || v == "1").unwrap_or(false);
    if token.is_none() && !allow_no_auth {
        anyhow::bail!("MCP_TOKEN is missing. Set a long secret token, or MCP_ALLOW_NO_AUTH=true for an isolated test network.");
    }
    if token.is_none() {
        warn!("running without authentication (MCP_ALLOW_NO_AUTH)");
    }

    // The server only reads; at startup it checks whether the database can be opened, but the
    // collector may start later (the database is then opened on the first request).
    match nibe_core::db::Reader::open(&db_path) {
        Ok(_) => info!("database {db_path:?} found"),
        Err(e) => warn!("database not available yet: {e:#}"),
    }

    let server = NibeServer::new(db_path, tz);
    let mut cfg = StreamableHttpServerConfig::default();
    // Host validation against DNS rebinding. Behind a router/reverse proxy the Host header is
    // the router's IP or host name; list it explicitly, or use "*" to skip the check
    // (the bearer token then remains the protection).
    match env("MCP_ALLOWED_HOSTS").as_deref() {
        Some("*") => cfg = cfg.disable_allowed_hosts(),
        Some(list) => {
            let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
            hosts.extend(list.split(',').map(|h| h.trim().to_string()).filter(|h| !h.is_empty()));
            cfg = cfg.with_allowed_hosts(hosts);
        }
        None => {
            if token.is_some() {
                cfg = cfg.disable_allowed_hosts();
            }
        }
    }
    let service = StreamableHttpService::new(move || Ok(server.clone()), LocalSessionManager::default().into(), cfg);

    let has_token = token.is_some();
    let auth = Auth { token: token.map(Arc::new) };
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(auth, require_token))
        .route("/healthz", get(|| async { "ok" }));

    // Stop on SIGINT or SIGTERM. Open MCP streams (SSE) would keep a graceful shutdown waiting
    // forever, so connections get 5 seconds to finish.
    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            info!("stopping");
            handle.graceful_shutdown(Some(Duration::from_secs(5)));
        });
    }
    let make_service = app.into_make_service();
    match (env("MCP_TLS_CERT"), env("MCP_TLS_KEY")) {
        (Some(cert), Some(key)) => {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| format!("loading MCP_TLS_CERT {cert:?} and MCP_TLS_KEY {key:?}"))?;
            info!("MCP listening on https://{bind}/mcp");
            axum_server::bind_rustls(bind, tls).handle(handle).serve(make_service).await.with_context(|| format!("serving on {bind}"))?;
        }
        (None, None) => {
            if has_token && !bind.ip().is_loopback() {
                warn!("serving plain HTTP on {bind}: the bearer token is readable by anyone who can see the traffic; set MCP_TLS_CERT and MCP_TLS_KEY");
            }
            info!("MCP listening on http://{bind}/mcp");
            axum_server::bind(bind).handle(handle).serve(make_service).await.with_context(|| format!("serving on {bind}"))?;
        }
        _ => anyhow::bail!("set both MCP_TLS_CERT and MCP_TLS_KEY, or neither"),
    }
    info!("stopped");
    Ok(())
}

/// Resolves on SIGINT or SIGTERM. Container runtimes stop with SIGTERM.
async fn shutdown_signal() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                warn!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await
            }
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term => {}
    }
}
