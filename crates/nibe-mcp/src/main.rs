//! Read-only MCP-server bovenop de SQLite-historie van de NIBE-collector.

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
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let db_path = env("DB_PATH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/data/nibe.db"));
    let bind: SocketAddr = env("MCP_BIND").unwrap_or_else(|| "0.0.0.0:8080".into()).parse().context("MCP_BIND")?;
    let tz = nibe_core::timeutil::parse_tz(&env("TZ_NAME").unwrap_or_else(|| "Europe/Amsterdam".into()))?;
    let token = env("MCP_TOKEN");
    let allow_no_auth = env("MCP_ALLOW_NO_AUTH").map(|v| v == "true" || v == "1").unwrap_or(false);
    if token.is_none() && !allow_no_auth {
        anyhow::bail!("MCP_TOKEN ontbreekt. Zet een lang geheim token, of MCP_ALLOW_NO_AUTH=true voor een afgeschermd test-netwerk.");
    }
    if token.is_none() {
        warn!("draait zonder authenticatie (MCP_ALLOW_NO_AUTH)");
    }

    // De server leest alleen; hij controleert bij de start of de database te openen is, maar de
    // collector mag later pas starten (de database wordt dan bij de eerste vraag geopend).
    match nibe_core::db::Reader::open(&db_path) {
        Ok(_) => info!("database {db_path:?} gevonden"),
        Err(e) => warn!("database nog niet beschikbaar: {e:#}"),
    }

    let server = NibeServer::new(db_path, tz);
    let mut cfg = StreamableHttpServerConfig::default();
    // Host-validatie tegen DNS-rebinding. Achter een router/reverse proxy is de Host-header
    // het IP of de hostnaam van de router; geef die expliciet op, of "*" om te negeren
    // (het bearer-token blijft dan de beveiliging).
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

    let auth = Auth { token: token.map(Arc::new) };
    let app = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(auth, require_token))
        .route("/healthz", get(|| async { "ok" }));

    let listener = tokio::net::TcpListener::bind(bind).await.with_context(|| format!("luisteren op {bind}"))?;
    info!("MCP luistert op http://{bind}/mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
