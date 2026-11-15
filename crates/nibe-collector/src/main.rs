//! Haalt periodiek alle datapunten op via de lokale NIBE REST-API en schrijft de
//! veranderingen naar SQLite. Alleen GET-verzoeken; er wordt nooit iets op de warmtepomp gewijzigd.

use anyhow::{anyhow, bail, Context, Result};
use nibe_core::api::{normalize_raw, parse_point_response, parse_points, ApiPoint};
use nibe_core::db::Store;
use nibe_core::timeutil::now_ms;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

struct Config {
    base_url: String,
    user: String,
    pass: String,
    device: u32,
    insecure_tls: bool,
    poll: Duration,
    fast_ids: Vec<i64>,
    fast: Duration,
    db_path: PathBuf,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    fn from_env() -> Result<Self> {
        let base_url = env("NIBE_URL").ok_or_else(|| anyhow!("NIBE_URL ontbreekt (bv. https://192.168.1.50:8443)"))?;
        let poll = env("NIBE_POLL_SECS").map(|v| v.parse::<u64>()).transpose().context("NIBE_POLL_SECS")?.unwrap_or(60);
        let fast = env("NIBE_FAST_SECS").map(|v| v.parse::<u64>()).transpose().context("NIBE_FAST_SECS")?.unwrap_or(10);
        if poll < 10 || fast < 2 {
            bail!("NIBE_POLL_SECS moet ≥ 10 zijn en NIBE_FAST_SECS ≥ 2; de pomp is geen database");
        }
        let fast_ids = env("NIBE_FAST_IDS")
            .map(|v| v.split(',').map(|s| s.trim().parse::<i64>()).collect::<Result<Vec<_>, _>>())
            .transpose()
            .context("NIBE_FAST_IDS moet een kommagescheiden lijst punt-id's zijn")?
            .unwrap_or_default();
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            user: env("NIBE_USER").ok_or_else(|| anyhow!("NIBE_USER ontbreekt"))?,
            pass: env("NIBE_PASS").ok_or_else(|| anyhow!("NIBE_PASS ontbreekt"))?,
            device: env("NIBE_DEVICE").map(|v| v.parse()).transpose()?.unwrap_or(0),
            insecure_tls: env("NIBE_INSECURE_TLS").map(|v| v != "false" && v != "0").unwrap_or(true),
            poll: Duration::from_secs(poll),
            fast_ids,
            fast: Duration::from_secs(fast),
            db_path: env("DB_PATH").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/data/nibe.db")),
        })
    }
}

/// Eén meetronde richting de schrijver-thread.
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
                    error!("catalogus bijwerken mislukt: {e:#}");
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
                        error!("samples schrijven mislukt: {e:#}");
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
            // het pollogboek is alleen voor gatendetectie; 90 dagen is ruim voldoende
            if let Err(e) = store.prune_polls(now - 90 * 86_400_000) {
                warn!("poll-log opschonen mislukt: {e:#}");
            }
        }
    }
    Ok(())
}

fn build_client(cfg: &Config) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(cfg.insecure_tls)
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(8))
        .pool_idle_timeout(Duration::from_secs(30))
        .user_agent("nibe-collector/0.1")
        .build()
        .context("HTTP-client bouwen")
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

async fn full_poll(client: &reqwest::Client, cfg: &Config, tx: &mpsc::Sender<Msg>, first: bool) -> Result<(usize, usize)> {
    let t0 = Instant::now();
    let ts_ms = now_ms();
    let body = match get(client, cfg, "/points").await {
        Ok(b) => b,
        Err(e) => {
            let _ = tx.send(Msg::Failure { ts_ms, kind: "full", duration_ms: t0.elapsed().as_millis() as i64, error: format!("{e:#}") }).await;
            return Err(e);
        }
    };
    let pts = match parse_points(&body) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Msg::Failure { ts_ms, kind: "full", duration_ms: t0.elapsed().as_millis() as i64, error: format!("parseren: {e:#}") }).await;
            return Err(e);
        }
    };
    let n = pts.len();
    let values = to_values(&pts);
    // de catalogus (titels, eenheden, schaal) wijzigt alleen bij een firmware-update: bij de start en dagelijks
    if first {
        tx.send(Msg::Catalog(pts)).await.map_err(|_| anyhow!("schrijver is gestopt"))?;
    }
    let (reply, rx) = oneshot::channel();
    let duration_ms = t0.elapsed().as_millis() as i64;
    tx.send(Msg::Sample { ts_ms, kind: "full", values, duration_ms, reply }).await.map_err(|_| anyhow!("schrijver is gestopt"))?;
    let changed = rx.await.unwrap_or(0);
    Ok((n, changed))
}

async fn fast_loop(client: reqwest::Client, cfg: std::sync::Arc<Config>, tx: mpsc::Sender<Msg>) {
    let mut tick = tokio::time::interval(cfg.fast);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut fails = 0u32;
    loop {
        tick.tick().await;
        let ts_ms = now_ms();
        let t0 = Instant::now();
        let mut values = Vec::new();
        let mut err = None;
        for id in &cfg.fast_ids {
            match get(&client, &cfg, &format!("/points/{id}")).await.and_then(|b| parse_point_response(&b)) {
                Ok(pts) => {
                    for p in pts {
                        values.push((p.metadata.variable_id, normalize_raw(&p.metadata.variable_size, p.value.is_ok, p.value.integer_value)));
                    }
                }
                Err(e) => {
                    err = Some(format!("{e:#}"));
                    break;
                }
            }
        }
        if let Some(e) = err {
            fails += 1;
            if fails == 1 || fails % 30 == 0 {
                warn!("snelle poll mislukt ({fails}x): {e}");
            }
            let _ = tx.send(Msg::Failure { ts_ms, kind: "fast", duration_ms: t0.elapsed().as_millis() as i64, error: e }).await;
            tokio::time::sleep(Duration::from_secs((fails as u64).min(30) * 2)).await;
            continue;
        }
        fails = 0;
        let (reply, rx) = oneshot::channel();
        if tx.send(Msg::Sample { ts_ms, kind: "fast", values, duration_ms: t0.elapsed().as_millis() as i64, reply }).await.is_err() {
            return;
        }
        let _ = rx.await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cfg = std::sync::Arc::new(Config::from_env()?);
    info!("NIBE {} (apparaat {}), poll {}s, db {:?}", cfg.base_url, cfg.device, cfg.poll.as_secs(), cfg.db_path);
    if cfg.insecure_tls {
        info!("TLS-certificaat wordt niet gecontroleerd (zelfondertekend certificaat van de pomp)");
    }
    let store = Store::open(&cfg.db_path)?;
    let (tx, rx) = mpsc::channel::<Msg>(64);
    let poll_secs = cfg.poll.as_secs();
    let writer = std::thread::Builder::new().name("writer".into()).spawn(move || writer_thread(store, rx, poll_secs))?;

    let client = build_client(&cfg)?;
    let mut tick = tokio::time::interval(cfg.poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut first = true;
    let mut fast_started = false;
    let mut fails = 0u32;
    let mut last_catalog = Instant::now();
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = &mut shutdown => { info!("stoppen"); break; }
        }
        let refresh_catalog = first || last_catalog.elapsed() > Duration::from_secs(86_400);
        match full_poll(&client, &cfg, &tx, refresh_catalog).await {
            Ok((n, changed)) => {
                if fails > 0 {
                    info!("verbinding hersteld na {fails} mislukte polls");
                }
                fails = 0;
                if refresh_catalog {
                    last_catalog = Instant::now();
                }
                if first {
                    info!("eerste poll: {n} punten, {changed} samples opgeslagen");
                } else {
                    tracing::debug!("{n} punten, {changed} veranderd");
                }
                first = false;
                if !fast_started && !cfg.fast_ids.is_empty() {
                    fast_started = true;
                    info!("snelle poll voor {:?} elke {}s", cfg.fast_ids, cfg.fast.as_secs());
                    tokio::spawn(fast_loop(client.clone(), cfg.clone(), tx.clone()));
                }
            }
            Err(e) => {
                fails += 1;
                if fails == 1 || fails % 10 == 0 {
                    warn!("poll mislukt ({fails}x op rij): {e:#}");
                }
            }
        }
    }
    drop(tx);
    let _ = writer.join();
    Ok(())
}
