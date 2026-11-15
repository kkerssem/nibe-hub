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
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
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

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// `nibe-mcp health`, for a container health check: exit code 0 when the server accepts TCP
/// connections on MCP_BIND's port (on localhost when MCP_BIND is 0.0.0.0 or [::]). A plain
/// connect, so it needs neither the token nor the TLS certificate.
fn health() -> i32 {
    let bind: SocketAddr = match env("MCP_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()).parse() {
        Ok(b) => b,
        Err(e) => {
            println!("unhealthy: MCP_BIND: {e}");
            return 1;
        }
    };
    let target = match bind.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::from((Ipv4Addr::LOCALHOST, bind.port())),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::from((Ipv6Addr::LOCALHOST, bind.port())),
        _ => bind,
    };
    match std::net::TcpStream::connect_timeout(&target, Duration::from_secs(5)) {
        Ok(_) => {
            println!("healthy: {target} accepts connections");
            0
        }
        Err(e) => {
            println!("unhealthy: cannot connect to {target}: {e}");
            1
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("health") => std::process::exit(health()),
        Some(other) => anyhow::bail!("unknown argument {other:?}: run without arguments, or with `health` for a health check"),
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let db_path = env("DB_PATH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/data/nibe.db"));
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
