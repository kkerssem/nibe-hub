//! Query layer on top of the store: time series, raw changes and the COP analysis.

use crate::db::{PointInfo, Reader};
use crate::series::{aggregate, build_segments, round, stat, Agg, Gap, Seg};
use crate::timeutil::{bucket_edges, fmt_ts, Bucket, MAX_BUCKETS};
use crate::wellknown as wk;
use anyhow::{bail, Result};
use chrono_tz::Tz;
use serde_json::{json, Value};

pub const MAX_IDS: usize = 12;
pub const MAX_RAW_ROWS: usize = 2000;

/// Gaps and end of the reliable data for the range `[from, to)`.
pub struct Coverage {
    pub gaps: Vec<Gap>,
    pub data_end: i64,
}

pub fn coverage(reader: &Reader, from: i64, to: i64) -> Result<Coverage> {
    let interval_ms = reader
        .meta("poll_interval_secs")?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(60)
        * 1000;
    let thresh = (3 * interval_ms).max(180_000);
    let polls = reader.ok_polls(from, to)?;
    let mut gaps = Vec::new();
    for w in polls.windows(2) {
        if w[1] - w[0] > thresh {
            gaps.push((w[0] + interval_ms, w[1]));
        }
    }
    let data_end = polls.last().map(|p| p + interval_ms).unwrap_or(i64::MIN);
    Ok(Coverage { gaps, data_end })
}

pub fn segments(reader: &Reader, info: &PointInfo, from: i64, to: i64, cov: &Coverage) -> Result<Vec<Seg>> {
    let samples: Vec<(i64, Option<f64>)> = reader
        .samples(info.id, from, to, None)?
        .into_iter()
        .map(|(t, r)| (t, r.map(|r| info.scale(r))))
        .collect();
    Ok(build_segments(&samples, from, to, cov.data_end, &cov.gaps))
}

fn cell(v: Option<f64>) -> Value {
    v.map(|x| json!(round(x))).unwrap_or(Value::Null)
}

fn effective_len(b: (i64, i64), from: i64, to: i64) -> f64 {
    (b.1.min(to) - b.0.max(from)).max(1) as f64
}

pub fn series_table(reader: &Reader, tz: Tz, ids: &[i64], from: i64, to: i64, bucket: Bucket, aggs: &[Agg]) -> Result<Value> {
    if ids.is_empty() {
        bail!("give at least one point id");
    }
    if ids.len() > MAX_IDS {
        bail!("at most {MAX_IDS} points per request");
    }
    let mut infos = Vec::new();
    for id in ids {
        match reader.point(*id)? {
            Some(i) => infos.push(i),
            None => bail!("point {id} does not exist; search with list_points"),
        }
    }
    let mut legend = serde_json::Map::new();
    for i in &infos {
        legend.insert(i.id.to_string(), json!({"title": i.title, "unit": i.unit}));
    }
    let cov = coverage(reader, from, to)?;

    if bucket == Bucket::Raw {
        // Each point gets an equal share of the row budget, and the limit is applied in SQL,
        // so a long range never loads more than the rows it returns.
        let per_point = MAX_RAW_ROWS / infos.len();
        let mut rows = Vec::new();
        let mut truncated = Vec::new();
        for info in &infos {
            let mut samples = reader.samples(info.id, from, to, Some(per_point + 1))?;
            if samples.len() > per_point {
                samples.truncate(per_point);
                truncated.push(info.id);
            }
            for (i, (t, raw)) in samples.into_iter().enumerate() {
                let t = if i == 0 && t < from { from } else { t };
                rows.push(json!([fmt_ts(t, tz), info.id, raw.map(|r| round(info.scale(r)))]));
            }
        }
        return Ok(json!({
            "timezone": tz.name(), "bucket": "raw", "series": legend,
            "columns": ["time", "id", "value"], "rows": rows,
            "notes": if truncated.is_empty() { vec![] } else {
                vec![format!("points {truncated:?} truncated at {per_point} rows each (the first rows of the range); choose a shorter range or a bucket")]
            },
        }));
    }

    let edges = bucket_edges(from, to, bucket, tz)?;
    let segs: Vec<Vec<Seg>> = infos.iter().map(|i| segments(reader, i, from, to, &cov)).collect::<Result<_>>()?;
    let aggs: Vec<Agg> = if aggs.is_empty() { vec![Agg::Avg] } else { aggs.to_vec() };

    let mut columns = vec![json!("time"), json!("coverage")];
    for i in &infos {
        for a in &aggs {
            columns.push(json!(format!("{}:{}", i.id, a.name())));
        }
    }
    let mut notes = Vec::new();
    if aggs.contains(&Agg::IntegralKwh) {
        for i in &infos {
            if i.unit != "W" && i.unit != "kW" {
                notes.push(format!("integral_kwh only works for W or kW; point {} is in {:?}", i.id, i.unit));
            }
        }
    }
    let mut rows = Vec::with_capacity(edges.len());
    for e in &edges {
        let len = effective_len(*e, from, to);
        let mut min_cov: f64 = 1.0;
        let mut vals = Vec::new();
        for (info, sg) in infos.iter().zip(&segs) {
            let st = stat(sg, e.0.max(from), e.1.min(to));
            min_cov = min_cov.min(st.covered_ms as f64 / len);
            for a in &aggs {
                vals.push(cell(aggregate(sg, e.0.max(from), e.1.min(to), *a, &info.unit)));
            }
        }
        // no empty rows before the start of the history
        if rows.is_empty() && vals.iter().all(Value::is_null) {
            continue;
        }
        let mut row = vec![json!(fmt_ts(e.0, tz)), json!((min_cov * 100.0).round() / 100.0)];
        row.extend(vals);
        rows.push(Value::Array(row));
    }
    notes.push("coverage = share of the bucket for which the collector has data (lowest of all requested points); averages and integrals only use measured time".into());
    Ok(json!({
        "timezone": tz.name(), "bucket": bucket.label(), "series": legend,
        "columns": columns, "rows": rows, "notes": notes,
    }))
}

/// Efficiency per bucket from the counter and power points of the S735C.
pub fn cop_table(reader: &Reader, tz: Tz, from: i64, to: i64, bucket: Bucket) -> Result<Value> {
    if bucket == Bucket::Raw {
        bail!("choose a bucket (e.g. 1h or 1d)");
    }
    let edges = bucket_edges(from, to, bucket, tz)?;
    if edges.len() > MAX_BUCKETS {
        bail!("too many buckets");
    }
    let cov = coverage(reader, from, to)?;
    let need = |id: i64| -> Result<(PointInfo, Vec<Seg>)> {
        let info = reader
            .point(id)?
            .ok_or_else(|| anyhow::anyhow!("point {id} is missing from the catalogue; is the collector running?"))?;
        let s = segments(reader, &info, from, to, &cov)?;
        Ok((info, s))
    };
    let (unit_i, unit_s) = need(wk::UNIT_POWER_W)?;
    let (comp_i, comp_s) = need(wk::COMPRESSOR_POWER_W)?;
    let (prod_i, prod_s) = need(wk::LIFETIME_PRODUCTION_KWH)?;
    let (cons_i, cons_s) = need(wk::LIFETIME_CONSUMPTION_KWH)?;
    let (heat_i, heat_s) = need(wk::HEATING_COMPRESSOR_ONLY_KWH)?;
    let (hw_i, hw_s) = need(wk::HOTWATER_COMPRESSOR_ONLY_KWH)?;
    let (out_i, out_s) = need(wk::OUTDOOR_BT1)?;
    let (exh_i, exh_s) = need(wk::EXHAUST_BT20)?;
    let (on_i, on_s) = need(wk::COMPRESSOR_STATUS)?;

    let columns = [
        "time", "coverage", "elec_unit_kwh", "elec_compressor_kwh", "counter_consumed_kwh", "counter_produced_kwh",
        "cop_counters", "cop_produced_vs_integrated", "counter_heating_kwh", "counter_hotwater_kwh",
        "outdoor_avg_c", "exhaust_avg_c", "compressor_on_fraction",
    ];
    let mut rows = Vec::new();
    for e in &edges {
        let (b0, b1) = (e.0.max(from), e.1.min(to));
        let len = effective_len(*e, from, to);
        let cov_frac = stat(&unit_s, b0, b1).covered_ms as f64 / len;
        let elec_unit = aggregate(&unit_s, b0, b1, Agg::IntegralKwh, &unit_i.unit);
        let elec_comp = aggregate(&comp_s, b0, b1, Agg::IntegralKwh, &comp_i.unit);
        let cons = aggregate(&cons_s, b0, b1, Agg::Delta, &cons_i.unit);
        let prod = aggregate(&prod_s, b0, b1, Agg::Delta, &prod_i.unit);
        let cop_counters = match (prod, cons) {
            (Some(p), Some(c)) if c >= 0.5 && p >= 0.0 => Some(p / c),
            _ => None,
        };
        // Only reliable if the unit's electricity was measured over (nearly) the whole bucket.
        let cop_integrated = match (prod, elec_unit) {
            (Some(p), Some(u)) if u >= 0.5 && p >= 0.0 && cov_frac > 0.9 => Some(p / u),
            _ => None,
        };
        let cells = [
            cell(elec_unit), cell(elec_comp), cell(cons), cell(prod), cell(cop_counters), cell(cop_integrated),
            cell(aggregate(&heat_s, b0, b1, Agg::Delta, &heat_i.unit)),
            cell(aggregate(&hw_s, b0, b1, Agg::Delta, &hw_i.unit)),
            cell(aggregate(&out_s, b0, b1, Agg::Avg, &out_i.unit)),
            cell(aggregate(&exh_s, b0, b1, Agg::Avg, &exh_i.unit)),
            cell(aggregate(&on_s, b0, b1, Agg::Avg, &on_i.unit)),
        ];
        if rows.is_empty() && cells.iter().all(Value::is_null) {
            continue; // no history in this bucket yet
        }
        let mut row = vec![json!(fmt_ts(e.0, tz)), json!((cov_frac * 100.0).round() / 100.0)];
        row.extend(cells);
        rows.push(Value::Array(row));
    }
    Ok(json!({
        "timezone": tz.name(), "bucket": bucket.label(),
        "columns": columns, "rows": rows,
        "notes": [
            "elec_unit_kwh = ∫ 'Instantaneous used power' (point 22130); elec_compressor_kwh = ∫ 'Compressor power input' (point 1801)",
            "counter_* = difference of the lifetime counters (resolution 0.1 kWh; points 28392/28393 and 27378/27379). The meaning of 28392/28393 is not validated yet: if counter_consumed_kwh ≈ elec_unit_kwh, it is correct.",
            "cop_counters = counter_produced / counter_consumed (only with ≥ 0.5 kWh consumed); cop_produced_vs_integrated = counter_produced / elec_unit_kwh (only with > 90% coverage)",
            "COP is based on NIBE's own heat estimate, not an independent heat meter"
        ],
    }))
}
