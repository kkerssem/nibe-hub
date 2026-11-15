//! Periodically fetches all data points from the local NIBE REST API and writes the
//! changes to SQLite. GET requests only; nothing on the heat pump is ever changed.

mod tls;

use anyhow::{anyhow, bail, Context, Result};
use nibe_core::api::{normalize_raw, parse_point_response, parse_points, ApiPoint};
use nibe_core::db::Store;
use nibe_core::timeutil::now_ms;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

/// How the pump's TLS certificate is checked.
enum TlsMode {
    /// Normal CA verification (for a pump behind a proxy with a real certificate).
    Verify,
    /// Accept only the certificate with this SHA-256 fingerprint.
    Pinned([u8; 32]),
    /// No verification at all (`NIBE_INSECURE_TLS=true`).
    Insecure,
}

struct Config {
    base_url: String,
    user: String,
    pass: String,
    device: u32,
    tls: TlsMode,
    poll: Duration,
    fast_ids: Vec<i64>,
    fast: Duration,
    db_path: PathBuf,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

/// A whole number from the environment, with an error that shows the offending value
/// (a trailing comment such as `60 # once a minute` is the usual cause).
fn env_num<T: std::str::FromStr>(key: &str, default: T) -> Result<T> {
    match env(key) {
        None => Ok(default),
        Some(v) => v.trim().parse().map_err(|_| anyhow!("{key}={v:?} is not a whole number (comments after a value are not allowed)")),
    }
}

impl Config {
    fn from_env() -> Result<Self> {
        let base_url = env("NIBE_URL").ok_or_else(|| anyhow!("NIBE_URL is missing (e.g. https://192.168.1.50:8443)"))?;
        // Use the parser's normalised form, so the logs show the address actually used
        // (e.g. a missing "//" in "https:192.168.1.50:8443" is accepted and corrected).
        let base_url = match reqwest::Url::parse(base_url.trim()) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()) => {
                u.as_str().trim_end_matches('/').to_string()
            }
            _ => bail!("NIBE_URL={base_url:?} is not a valid URL; expected something like https://192.168.1.50:8443"),
        };
        let poll: u64 = env_num("NIBE_POLL_SECS", 60)?;
        let fast: u64 = env_num("NIBE_FAST_SECS", 10)?;
        if poll < 10 || fast < 2 {
            bail!("NIBE_POLL_SECS must be ≥ 10 and NIBE_FAST_SECS ≥ 2; the pump is not a database");
        }
        let fast_ids = env("NIBE_FAST_IDS")
            .map(|v| v.split(',').map(|s| s.trim().parse::<i64>()).collect::<Result<Vec<_>, _>>())
            .transpose()
            .context("NIBE_FAST_IDS must be a comma-separated list of point ids")?
            .unwrap_or_default();
        let insecure = env("NIBE_INSECURE_TLS").map(|v| v == "true" || v == "1").unwrap_or(false);
        // An empty NIBE_TLS_FINGERPRINT= line (as in the template) is a setting left unfilled,
        // not a request for CA verification, which always fails against the pump.
        if std::env::var("NIBE_TLS_FINGERPRINT").is_ok_and(|v| v.trim().is_empty()) && !insecure {
            bail!("NIBE_TLS_FINGERPRINT is empty: fill in the pump's certificate fingerprint (see README), or remove the line to verify against the system CAs");
        }
        let tls = match env("NIBE_TLS_FINGERPRINT") {
            Some(fp) => {
                if insecure {
                    bail!("set either NIBE_TLS_FINGERPRINT or NIBE_INSECURE_TLS, not both");
                }
                TlsMode::Pinned(tls::parse_fingerprint(&fp)?)
            }
            None if insecure => TlsMode::Insecure,
            None => TlsMode::Verify,
        };
        Ok(Self {
            base_url,
            user: env("NIBE_USER").ok_or_else(|| anyhow!("NIBE_USER is missing"))?,
            pass: env("NIBE_PASS").ok_or_else(|| anyhow!("NIBE_PASS is missing"))?,
            device: env_num("NIBE_DEVICE", 0)?,
            tls,
            poll: Duration::from_secs(poll),
            fast_ids,
            fast: Duration::from_secs(fast),
            db_path: env("DB_PATH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/data/nibe.db")),
        })
    }
}

/// One measurement round sent to the writer thread.
enum Msg {
    Catalog(BTreeMap<i64, ApiPoint>),
    Sample {
        ts_ms: i64,
        kind: &'static str,
        values: Vec<(i64, Option<i64>)>,
        duration_ms: i64,
        reply: oneshot::Sender<usize>,
    },
    Failure { ts_ms: i64, kind: &'static str, duration_ms: i64, error: String },
}

fn writer_thread(mut store: Store, mut rx: mpsc::Receiver<Msg>, poll_secs: u64) -> Result<()> {
    store.set_meta("poll_interval_secs", &poll_secs.to_string())?;
    let mut last_prune = 0i64;
    while let Some(msg) = rx.blocking_recv() {
        match msg {
            Msg::Catalog(pts) => {
                if let Err(e) = store.upsert_catalog(&pts, now_ms()) {
                    error!("updating catalogue failed: {e:#}");
                }
            }
            Msg::Sample { ts_ms, kind, values, duration_ms, reply } => {
                let n = values.len();
                match store.record(ts_ms, &values) {
                    Ok(changed) => {
                        if kind == "full" {
                            store.log_poll(ts_ms, kind, true, n, changed, duration_ms, None);
                        }
                        let _ = reply.send(changed);
                    }
                    Err(e) => {
                        error!("writing samples failed: {e:#}");
                        store.log_poll(ts_ms, kind, false, n, 0, duration_ms, Some(&format!("{e:#}")));
                        let _ = reply.send(0);
                    }
                }
            }
            Msg::Failure { ts_ms, kind, duration_ms, error } => {
                store.log_poll(ts_ms, kind, false, 0, 0, duration_ms, Some(&error));
            }
        }
        let now = now_ms();
        if now - last_prune > 86_400_000 {
            last_prune = now;
            // successful full polls are kept (gap detection needs them); failures go after 90 days
            if let Err(e) = store.prune_polls(now - 90 * 86_400_000) {
                warn!("pruning poll log failed: {e:#}");
            }
        }
    }
    Ok(())
}

fn build_client(cfg: &Config) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(8))
        .pool_idle_timeout(Duration::from_secs(30))
        .user_agent("nibe-collector/0.1");
    let builder = match &cfg.tls {
        TlsMode::Verify => builder,
        TlsMode::Pinned(fp) => builder.tls_backend_preconfigured(tls::pinned_client_config(*fp)?),
        TlsMode::Insecure => builder.danger_accept_invalid_certs(true),
    };
    builder.build().context("building HTTP client")
}

async fn get(client: &reqwest::Client, cfg: &Config, path: &str) -> Result<Vec<u8>> {
    let url = format!("{}/api/v1/devices/{}{}", cfg.base_url, cfg.device, path);
    let resp = client.get(&url).basic_auth(&cfg.user, Some(&cfg.pass)).send().await.with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url}: HTTP {status}");
    }
    Ok(resp.bytes().await?.to_vec())
}

fn to_values(pts: &BTreeMap<i64, ApiPoint>) -> Vec<(i64, Option<i64>)> {
    pts.iter().map(|(id, p)| (*id, p.raw())).collect()
}

/// One full poll. Returns the number of points, the number of changed values, and the ids
/// the pump reported.
async fn full_poll(client: &reqwest::Client, cfg: &Config, tx: &mpsc::Sender<Msg>, first: bool) -> Result<(usize, usize, Vec<i64>)> {
    let t0 = Instant::now();
    let body = match get(client, cfg, "/points").await {
        Ok(b) => b,
        Err(e) => {
            let _ = tx.send(Msg::Failure { ts_ms: now_ms(), kind: "full", duration_ms: t0.elapsed().as_millis() as i64, error: format!("{e:#}") }).await;
            return Err(e);
        }
    };
    // Stamp the values when they arrived, not when the request was sent: a fast poll that
    // completed in the meantime then stays older than this one.
    let ts_ms = now_ms();
    let pts = match parse_points(&body) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Msg::Failure { ts_ms, kind: "full", duration_ms: t0.elapsed().as_millis() as i64, error: format!("parsing: {e:#}") }).await;
            return Err(e);
        }
    };
    let n = pts.len();
    let ids: Vec<i64> = pts.keys().copied().collect();
    let values = to_values(&pts);
    // the catalogue (titles, units, scale) only changes with a firmware update: refresh at startup and daily
    if first {
        tx.send(Msg::Catalog(pts)).await.map_err(|_| anyhow!("writer has stopped"))?;
    }
    let (reply, rx) = oneshot::channel();
    let duration_ms = t0.elapsed().as_millis() as i64;
    tx.send(Msg::Sample { ts_ms, kind: "full", values, duration_ms, reply }).await.map_err(|_| anyhow!("writer has stopped"))?;
    let changed = rx.await.unwrap_or(0);
    Ok((n, changed, ids))
}

/// Splits the configured fast-poll ids into those the pump knows and those it does not.
fn check_fast_ids(wanted: &[i64], known: &[i64]) -> (Vec<i64>, Vec<i64>) {
    wanted.iter().partition(|id| known.binary_search(id).is_ok())
}

/// Polls `ids` one by one. A failing id is skipped for that round instead of discarding
/// the values of the others; the round only counts as failed when no id succeeded.
async fn fast_loop(client: reqwest::Client, cfg: std::sync::Arc<Config>, ids: Vec<i64>, tx: mpsc::Sender<Msg>) {
    let mut tick = tokio::time::interval(cfg.fast);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut fails = 0u32;
    let mut partial = 0u32;
    loop {
        tick.tick().await;
        let t0 = Instant::now();
        let mut values = Vec::new();
        let mut errors = Vec::new();
        for id in &ids {
            let res = get(&client, &cfg, &format!("/points/{id}")).await.and_then(|b| parse_point_response(&b));
            match res {
                Ok(pts) => {
                    for p in pts {
                        values.push((p.metadata.variable_id, normalize_raw(&p.metadata.variable_size, p.value.is_ok, p.value.integer_value)));
                    }
                }
                Err(e) => errors.push(format!("point {id}: {e:#}")),
            }
        }
        let ts_ms = now_ms();
        let duration_ms = t0.elapsed().as_millis() as i64;
        if values.is_empty() {
            fails += 1;
            let e = errors.join("; ");
            if fails == 1 || fails % 30 == 0 {
                warn!("fast poll failed ({fails}x): {e}");
            }
            let _ = tx.send(Msg::Failure { ts_ms, kind: "fast", duration_ms, error: e }).await;
            tokio::time::sleep(Duration::from_secs((fails as u64).min(30) * 2)).await;
            continue;
        }
        fails = 0;
        if errors.is_empty() {
            partial = 0;
        } else {
            partial += 1;
            let e = errors.join("; ");
            if partial == 1 || partial % 30 == 0 {
                warn!("fast poll: {} of {} points failed ({partial}x): {e}", errors.len(), ids.len());
            }
            let _ = tx.send(Msg::Failure { ts_ms, kind: "fast", duration_ms, error: e }).await;
        }
        let (reply, rx) = oneshot::channel();
        if tx.send(Msg::Sample { ts_ms, kind: "fast", values, duration_ms, reply }).await.is_err() {
            return;
        }
        let _ = rx.await;
    }
}

/// Resolves on SIGINT or SIGTERM. Container runtimes stop with SIGTERM, which PID 1 would
/// otherwise ignore.
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

/// `nibe-collector health`, for a container health check: exit code 0 while the last
/// successful full poll in the database is recent (the same rule as `healthy` in the MCP
/// `status` tool). Reads the database only, so it needs just DB_PATH, not the pump settings.
fn health() -> i32 {
    let db_path = env("DB_PATH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/data/nibe.db"));
    let stats = match nibe_core::db::Reader::open(&db_path).and_then(|r| r.stats()) {
        Ok(s) => s,
        Err(e) => {
            println!("unhealthy: {e:#}");
            return 1;
        }
    };
    let now = now_ms();
    let healthy = stats.collector_healthy(now);
    let interval = stats.poll_interval_secs.map_or("unknown".to_string(), |i| format!("{i} s"));
    match stats.last_ok_poll_age_secs(now) {
        Some(age) => println!(
            "{}: last successful poll {age} s ago, poll interval {interval}",
            if healthy { "healthy" } else { "unhealthy" }
        ),
        None => println!("unhealthy: no successful poll yet"),
    }
    if !healthy {
        if let Some(e) = &stats.last_poll_error {
            println!("last poll error: {e}");
        }
    }
    if healthy { 0 } else { 1 }
}

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("health") => std::process::exit(health()),
        Some(other) => bail!("unknown argument {other:?}: run without arguments, or with `health` for a health check"),
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = std::sync::Arc::new(Config::from_env()?);
    info!("NIBE {} (device {}), poll {}s, db {:?}", cfg.base_url, cfg.device, cfg.poll.as_secs(), cfg.db_path);
    let url_is_ip = reqwest::Url::parse(&cfg.base_url)
        .ok()
        .is_some_and(|u| matches!(u.host(), Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_))));
    match &cfg.tls {
        TlsMode::Verify if url_is_ip && cfg.base_url.starts_with("https:") => warn!(
            "NIBE_URL is an IP address and NIBE_TLS_FINGERPRINT is not set: CA verification will fail against the pump's self-signed certificate; set NIBE_TLS_FINGERPRINT (see README)"
        ),
        TlsMode::Verify => info!("TLS certificate is verified against the system CAs; set NIBE_TLS_FINGERPRINT for the pump's self-signed certificate"),
        TlsMode::Pinned(fp) => info!("TLS certificate pinned to SHA-256 {}", tls::fmt_fingerprint(fp)),
        TlsMode::Insecure => warn!("TLS certificate is NOT verified (NIBE_INSECURE_TLS); the pump password goes to whoever answers at NIBE_URL"),
    }
    let store = Store::open(&cfg.db_path)?;
    let (tx, rx) = mpsc::channel::<Msg>(64);
    let poll_secs = cfg.poll.as_secs();
    let writer = std::thread::Builder::new().name("writer".into()).spawn(move || writer_thread(store, rx, poll_secs))?;

    let client = build_client(&cfg)?;
    let mut tick = tokio::time::interval(cfg.poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut first = true;
    let mut fast_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut fast_checked = cfg.fast_ids.is_empty();
    let mut fails = 0u32;
    let mut last_catalog = Instant::now();
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = &mut shutdown => { info!("stopping"); break; }
        }
        let refresh_catalog = first || last_catalog.elapsed() > Duration::from_secs(86_400);
        let result = tokio::select! {
            r = full_poll(&client, &cfg, &tx, refresh_catalog) => r,
            _ = &mut shutdown => { info!("stopping"); break; }
        };
        match result {
            Ok((n, changed, known_ids)) => {
                if fails > 0 {
                    info!("connection restored after {fails} failed polls");
                }
                fails = 0;
                if refresh_catalog {
                    last_catalog = Instant::now();
                }
                if first {
                    info!("first poll: {n} points, {changed} samples stored");
                } else {
                    tracing::debug!("{n} points, {changed} changed");
                }
                first = false;
                if !fast_checked {
                    // check the ids against what the pump actually reports, once
                    fast_checked = true;
                    let (ok, unknown) = check_fast_ids(&cfg.fast_ids, &known_ids);
                    if !unknown.is_empty() {
                        warn!("NIBE_FAST_IDS: the pump has no point {unknown:?}; fast polling skips it (check for a typo)");
                    }
                    if ok.is_empty() {
                        warn!("NIBE_FAST_IDS: none of the ids exist; fast polling is off");
                    } else {
                        info!("fast poll for {ok:?} every {}s", cfg.fast.as_secs());
                        fast_task = Some(tokio::spawn(fast_loop(client.clone(), cfg.clone(), ok, tx.clone())));
                    }
                }
            }
            Err(e) => {
                fails += 1;
                if fails == 1 || fails % 10 == 0 {
                    warn!("poll failed ({fails}x in a row): {e:#}");
                    if matches!(cfg.tls, TlsMode::Verify) && format!("{e:#}").to_lowercase().contains("certificate") {
                        warn!("the pump uses a self-signed certificate: set NIBE_TLS_FINGERPRINT (see README)");
                    }
                }
            }
        }
    }
    // The fast-poll task holds a sender; stop it so the writer sees the channel close and
    // finishes the pending writes.
    if let Some(t) = fast_task {
        t.abort();
        let _ = t.await;
    }
    drop(tx);
    let _ = tokio::task::spawn_blocking(move || writer.join()).await;
    info!("stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fast_ids_are_split_off() {
        let known = [4, 1801, 22130]; // sorted, as the full poll returns them
        assert_eq!(check_fast_ids(&[1801, 2213], &known), (vec![1801], vec![2213]));
        assert_eq!(check_fast_ids(&[22130, 1801], &known), (vec![22130, 1801], vec![]));
        assert_eq!(check_fast_ids(&[9], &known), (vec![], vec![9]));
    }
}
