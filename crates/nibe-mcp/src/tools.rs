use anyhow::Result;
use chrono_tz::Tz;
use nibe_core::analysis;
use nibe_core::db::Reader;
use nibe_core::series::{round, Agg};
use nibe_core::timeutil::{fmt_ts, now_ms, parse_bucket, parse_time};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub struct NibeServer {
    db_path: Arc<PathBuf>,
    tz: Tz,
}

impl NibeServer {
    pub fn new(db_path: PathBuf, tz: Tz) -> Self {
        Self { db_path: Arc::new(db_path), tz }
    }

    /// Voert een (blokkerende) databasequery uit en zet fouten om in een leesbare tool-fout.
    async fn run<F>(&self, f: F) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(&Reader, Tz, i64) -> Result<Value> + Send + 'static,
    {
        let path = self.db_path.clone();
        let tz = self.tz;
        let res = tokio::task::spawn_blocking(move || {
            let reader = Reader::open(&path)?;
            f(&reader, tz, now_ms())
        })
        .await
        .map_err(|e| McpError::internal_error(format!("taak afgebroken: {e}"), None))?;
        Ok(match res {
            Ok(v) => CallToolResult::success(vec![ContentBlock::text(serde_json::to_string(&v).unwrap_or_default())]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
        })
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListArgs {
    /// Woorden uit de titel (alle moeten voorkomen), bv. "compressor power" of "BT20". Een getal zoekt ook op punt-id.
    pub search: Option<String>,
    /// Alleen punten met deze eenheid, bv. "°C", "W", "kWh", "Hz", "m³/h".
    pub unit: Option<String>,
    /// Maximaal aantal resultaten (standaard 50, maximaal 500).
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CurrentArgs {
    /// Punt-id's, bv. [4, 22, 1801]. Laat leeg om op `search` te zoeken.
    pub ids: Option<Vec<i64>>,
    /// Titelwoorden als alternatief voor `ids`; geeft maximaal 40 punten.
    pub search: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DescribeArgs {
    /// Punt-id.
    pub id: i64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SeriesArgs {
    /// 1 tot 12 punt-id's.
    pub ids: Vec<i64>,
    /// Begin: "-24h", "-7d", "today", "yesterday", "2026-11-01", "2026-11-01 08:00" (lokale tijd) of RFC 3339.
    pub from: String,
    /// Einde (standaard "now").
    pub to: Option<String>,
    /// "raw" (alle wijzigingen), "1m", "5m", "15m", "30m", "1h", "3h", "6h", "12h", "1d", "1w" of "1mo". Standaard "1h".
    pub bucket: Option<String>,
    /// Een of meer van "avg", "min", "max", "first", "last", "delta" (voor tellers) en "integral_kwh" (voor W/kW-punten). Standaard ["avg"].
    pub aggs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CopArgs {
    /// Begin, zelfde notatie als bij `series`.
    pub from: String,
    /// Einde (standaard "now").
    pub to: Option<String>,
    /// "1h", "1d", "1w" of "1mo". Standaard "1d".
    pub bucket: Option<String>,
}

#[tool_router]
impl NibeServer {
    #[tool(description = "Health of the collector and the database: last successful poll, number of points and samples, database size, history range and the age of the newest data. Call this first when data looks stale.")]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        self.run(|r, tz, now| {
            let s = r.stats()?;
            let age = s.last_ok_poll_ms.map(|t| (now - t) / 1000);
            let healthy = match (age, s.poll_interval_secs) {
                (Some(a), Some(i)) => a <= 3 * i + 30,
                (Some(a), None) => a <= 240,
                _ => false,
            };
            Ok(json!({
                "healthy": healthy,
                "timezone": tz.name(),
                "now": fmt_ts(now, tz),
                "last_ok_poll": s.last_ok_poll_ms.map(|t| fmt_ts(t, tz)),
                "last_ok_poll_age_secs": age,
                "last_poll_ok": s.last_poll_ok,
                "last_poll_error": s.last_poll_error,
                "poll_interval_secs": s.poll_interval_secs,
                "points": s.points,
                "samples": s.samples,
                "history_from": s.first_sample_ms.map(|t| fmt_ts(t, tz)),
                "db_megabytes": (s.db_bytes as f64 / 1_048_576.0 * 10.0).round() / 10.0,
            }))
        })
        .await
    }

    #[tool(description = "Search the catalogue of NIBE data points by title words, unit or id. Returns id, title, unit, scale divisor, whether the point is writable on the heat pump (this server never writes) and the Modbus register.")]
    async fn list_points(&self, Parameters(a): Parameters<ListArgs>) -> Result<CallToolResult, McpError> {
        self.run(move |r, _, _| {
            let pts = r.search(a.search.as_deref(), a.unit.as_deref(), a.limit.unwrap_or(50))?;
            Ok(json!({"count": pts.len(), "points": pts.iter().map(|p| json!({
                "id": p.id, "title": p.title, "unit": p.unit, "divisor": p.divisor,
                "writable": p.writable, "modbus_register": p.modbus_register,
            })).collect::<Vec<_>>()}))
        })
        .await
    }

    #[tool(description = "Full catalogue entry for one point, including NIBE's description of the value coding (for example '20 = Stopped, 40 = Starting, 60 = Running') and its latest value.")]
    async fn describe_point(&self, Parameters(a): Parameters<DescribeArgs>) -> Result<CallToolResult, McpError> {
        self.run(move |r, tz, _| {
            let p = r.point(a.id)?.ok_or_else(|| anyhow::anyhow!("punt {} bestaat niet", a.id))?;
            let latest = r.latest(a.id)?;
            Ok(json!({
                "point": p,
                "latest_value": latest.and_then(|(_, raw)| raw).map(|raw| round(p.scale(raw))),
                "value_last_changed": latest.map(|(t, _)| fmt_ts(t, tz)),
            }))
        })
        .await
    }

    #[tool(description = "Latest known values (scaled to their unit) for the given point ids, or for points matching a title search. 'since' is when the value last changed, not when it was last measured; use status for the measurement time. A null value means the sensor is not connected.")]
    async fn current(&self, Parameters(a): Parameters<CurrentArgs>) -> Result<CallToolResult, McpError> {
        self.run(move |r, tz, _| {
            let infos = match (a.ids, a.search) {
                (Some(ids), _) if !ids.is_empty() => {
                    anyhow::ensure!(ids.len() <= 60, "maximaal 60 punten tegelijk");
                    let mut v = Vec::new();
                    for id in ids {
                        v.push(r.point(id)?.ok_or_else(|| anyhow::anyhow!("punt {id} bestaat niet"))?);
                    }
                    v
                }
                (_, Some(q)) => r.search(Some(&q), None, 40)?,
                _ => anyhow::bail!("geef `ids` of `search` op"),
            };
            let stats = r.stats()?;
            let values: Vec<Value> = infos
                .iter()
                .map(|p| {
                    let l = r.latest(p.id).ok().flatten();
                    json!({
                        "id": p.id, "title": p.title, "unit": p.unit,
                        "value": l.and_then(|(_, raw)| raw).map(|raw| round(p.scale(raw))),
                        "since": l.map(|(t, _)| fmt_ts(t, tz)),
                    })
                })
                .collect();
            Ok(json!({"data_as_of": stats.last_ok_poll_ms.map(|t| fmt_ts(t, tz)), "values": values}))
        })
        .await
    }

    #[tool(description = "Time series for 1-12 points, aggregated per time bucket with time-weighted averages (the store only keeps changes, so plain sample averages would be wrong). Use bucket 'raw' for every recorded change. Use aggs 'delta' for cumulative kWh counters and 'integral_kwh' for power points in W or kW. 'coverage' (0-1) shows how much of each bucket the collector actually measured; gaps are never filled in.")]
    async fn series(&self, Parameters(a): Parameters<SeriesArgs>) -> Result<CallToolResult, McpError> {
        self.run(move |r, tz, now| {
            let from = parse_time(&a.from, now, tz)?;
            let to = parse_time(a.to.as_deref().unwrap_or("now"), now, tz)?;
            let bucket = parse_bucket(a.bucket.as_deref().unwrap_or("1h"))?;
            let aggs = a.aggs.unwrap_or_default().iter().map(|s| Agg::parse(s)).collect::<Result<Vec<_>>>()?;
            analysis::series_table(r, tz, &a.ids, from, to, bucket, &aggs)
        })
        .await
    }

    #[tool(description = "Efficiency overview per hour/day/week/month for the NIBE S735C: electricity from integrated power (unit and compressor), the cumulative lifetime counters' deltas, COP from the counters and from the integrated power, heating vs hot water counter deltas, average outdoor and exhaust-air temperature and the compressor duty fraction. The COP is NIBE's own heat estimate, not an independent heat meter. Read the 'notes' in the result before drawing conclusions.")]
    async fn cop(&self, Parameters(a): Parameters<CopArgs>) -> Result<CallToolResult, McpError> {
        self.run(move |r, tz, now| {
            let from = parse_time(&a.from, now, tz)?;
            let to = parse_time(a.to.as_deref().unwrap_or("now"), now, tz)?;
            let bucket = parse_bucket(a.bucket.as_deref().unwrap_or("1d"))?;
            analysis::cop_table(r, tz, from, to, bucket)
        })
        .await
    }
}

#[tool_handler(
    name = "nibe-history",
    version = "0.1.0",
    instructions = "Read-only history of a NIBE S735C-7 exhaust-air heat pump, collected every minute from its local REST API. Workflow: call `status` to check data freshness, `list_points`/`describe_point` to find point ids, `current` for latest values, `series` for history (time-weighted aggregation per bucket) and `cop` for efficiency per hour/day. Values are already scaled to their unit (the catalogue's divisor has been applied). The store only keeps value changes, so a flat line is a value that did not change, and gaps appear as a coverage below 1 rather than as invented data. Useful points: 4 outdoor temperature BT1, 22 exhaust air BT20 (the heat source), 23 air after the evaporator BT21, 1801 compressor power in W, 22130 total unit power in W, 1799 compressor frequency in Hz, 5205 real air flow in m3/h, 5952 fan speed in percent, 781 degree minutes, 1973 compressor status (1 = on)."
)]
impl ServerHandler for NibeServer {}
