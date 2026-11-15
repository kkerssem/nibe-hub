//! Time-weighted aggregation of samples that are stored only when they change.
//!
//! A sample holds until the next sample (step function). Periods in which the collector
//! did not measure (gaps) are excluded, so an average or integral never rests on
//! invented data.

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Seg {
    pub start: i64,
    pub end: i64,
    pub v: f64,
}

pub type Gap = (i64, i64);

/// Builds segments over `[from, min(to, data_end))`. `samples` is ascending; the first
/// sample may lie before `from` (baseline). Missing values (`None`) produce no segment.
pub fn build_segments(samples: &[(i64, Option<f64>)], from: i64, to: i64, data_end: i64, gaps: &[Gap]) -> Vec<Seg> {
    let cap = to.min(data_end);
    let mut segs = Vec::new();
    for (i, (ts, v)) in samples.iter().enumerate() {
        let start = (*ts).max(from);
        let end = samples.get(i + 1).map(|n| n.0).unwrap_or(cap).min(cap);
        if let Some(v) = v {
            if end > start {
                segs.push(Seg { start, end, v: *v });
            }
        }
    }
    subtract_gaps(segs, gaps)
}

fn subtract_gaps(segs: Vec<Seg>, gaps: &[Gap]) -> Vec<Seg> {
    if gaps.is_empty() {
        return segs;
    }
    let mut out = Vec::with_capacity(segs.len());
    for s in segs {
        let mut cur = s.start;
        for &(g0, g1) in gaps {
            if g1 <= cur || g0 >= s.end {
                continue;
            }
            if g0 > cur {
                out.push(Seg { start: cur, end: g0.min(s.end), v: s.v });
            }
            cur = cur.max(g1);
            if cur >= s.end {
                break;
            }
        }
        if cur < s.end {
            out.push(Seg { start: cur, end: s.end, v: s.v });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Avg,
    Min,
    Max,
    First,
    Last,
    /// difference over the bucket, meant for counters (kWh meter readings)
    Delta,
    /// ∫ value dt converted to kWh; only for points in W or kW
    IntegralKwh,
}

impl Agg {
    pub fn parse(s: &str) -> Result<Agg> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "avg" | "mean" => Agg::Avg,
            "min" => Agg::Min,
            "max" => Agg::Max,
            "first" => Agg::First,
            "last" => Agg::Last,
            "delta" => Agg::Delta,
            "integral_kwh" | "kwh" => Agg::IntegralKwh,
            o => bail!("unknown aggregation {o:?}; choose avg, min, max, first, last, delta or integral_kwh"),
        })
    }
    pub fn name(&self) -> &'static str {
        match self {
            Agg::Avg => "avg",
            Agg::Min => "min",
            Agg::Max => "max",
            Agg::First => "first",
            Agg::Last => "last",
            Agg::Delta => "delta",
            Agg::IntegralKwh => "integral_kwh",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Stat {
    pub covered_ms: i64,
    /// Σ value × duration (value·ms)
    pub integral: f64,
    pub min: f64,
    pub max: f64,
    pub first: Option<f64>,
    pub last: Option<f64>,
}

pub fn stat(segs: &[Seg], b0: i64, b1: i64) -> Stat {
    let mut s = Stat { covered_ms: 0, integral: 0.0, min: f64::INFINITY, max: f64::NEG_INFINITY, first: None, last: None };
    let mut i = segs.partition_point(|g| g.end <= b0);
    while i < segs.len() && segs[i].start < b1 {
        let g = segs[i];
        let (a, b) = (g.start.max(b0), g.end.min(b1));
        if b > a {
            s.covered_ms += b - a;
            s.integral += g.v * (b - a) as f64;
            s.min = s.min.min(g.v);
            s.max = s.max.max(g.v);
            s.first.get_or_insert(g.v);
            s.last = Some(g.v);
        }
        i += 1;
    }
    s
}

pub fn value_at(segs: &[Seg], t: i64) -> Option<f64> {
    let i = segs.partition_point(|g| g.end <= t);
    segs.get(i).filter(|g| g.start <= t).map(|g| g.v)
}

/// `unit` is the unit of the point; only needed for `IntegralKwh`.
pub fn aggregate(segs: &[Seg], b0: i64, b1: i64, agg: Agg, unit: &str) -> Option<f64> {
    let st = stat(segs, b0, b1);
    if st.covered_ms == 0 {
        return None;
    }
    match agg {
        Agg::Avg => Some(st.integral / st.covered_ms as f64),
        Agg::Min => Some(st.min),
        Agg::Max => Some(st.max),
        Agg::First => st.first,
        Agg::Last => st.last,
        Agg::Delta => {
            let start = value_at(segs, b0).or(st.first)?;
            let end = value_at(segs, b1).or(st.last)?;
            Some(end - start)
        }
        Agg::IntegralKwh => match unit {
            "W" => Some(st.integral / 3.6e9),
            "kW" => Some(st.integral / 3.6e6),
            _ => None,
        },
    }
}

/// Increase of a cumulative counter over `[b0, b1]`, from the same boundary values as
/// [`Agg::Delta`], but counting only rising steps. A drop is a reset of the counter (for
/// example the pump's energy log cleared from its menu) and adds nothing, so a reset costs at
/// most one poll's worth of increase instead of producing a large negative value.
/// Returns the increase and whether a reset was seen; `None` without measured data.
pub fn counter_increase(segs: &[Seg], b0: i64, b1: i64) -> Option<(f64, bool)> {
    let st = stat(segs, b0, b1);
    if st.covered_ms == 0 {
        return None;
    }
    let mut prev = value_at(segs, b0).or(st.first)?;
    let (mut increase, mut reset) = (0.0, false);
    let first = segs.partition_point(|g| g.end <= b0);
    for g in segs[first..].iter().take_while(|g| g.start <= b1) {
        if g.start <= b0 {
            continue;
        }
        if g.v >= prev {
            increase += g.v - prev;
        } else {
            reset = true;
        }
        prev = g.v;
    }
    Some((increase, reset))
}

pub fn round(v: f64) -> f64 {
    if v.is_finite() {
        (v * 1000.0).round() / 1000.0
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_function_is_time_weighted() {
        // 100 W for 30 min, then 300 W for 30 min => average 200 W, 0.2 kWh
        let samples = [(0, Some(100.0)), (1_800_000, Some(300.0))];
        let segs = build_segments(&samples, 0, 3_600_000, 3_600_000, &[]);
        assert_eq!(aggregate(&segs, 0, 3_600_000, Agg::Avg, "W"), Some(200.0));
        let kwh = aggregate(&segs, 0, 3_600_000, Agg::IntegralKwh, "W").unwrap();
        assert!((kwh - 0.2).abs() < 1e-9);
    }

    #[test]
    fn gaps_are_excluded_not_filled() {
        let samples = [(0, Some(100.0))];
        let segs = build_segments(&samples, 0, 1000, 1000, &[(200, 800)]);
        assert_eq!(stat(&segs, 0, 1000).covered_ms, 400);
    }

    #[test]
    fn counter_delta_uses_bucket_boundaries() {
        let samples = [(0, Some(10.0)), (500, Some(10.4)), (1500, Some(10.9))];
        let segs = build_segments(&samples, 0, 2000, 2000, &[]);
        assert!((aggregate(&segs, 0, 1000, Agg::Delta, "kWh").unwrap() - 0.4).abs() < 1e-9);
        assert!((aggregate(&segs, 1000, 2000, Agg::Delta, "kWh").unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn counter_increase_skips_a_reset() {
        // 100.0 -> 100.4, reset to 0.1, -> 0.3: increase 0.4 + 0.2, the drop adds nothing
        let samples = [(0, Some(100.0)), (500, Some(100.4)), (1000, Some(0.1)), (1500, Some(0.3))];
        let segs = build_segments(&samples, 0, 2000, 2000, &[]);
        let (inc, reset) = counter_increase(&segs, 0, 2000).unwrap();
        assert!((inc - 0.6).abs() < 1e-9 && reset, "{inc} {reset}");
        // without a reset it equals Delta, including a step exactly at the bucket end
        let (inc, reset) = counter_increase(&segs, 0, 500).unwrap();
        assert!((inc - 0.4).abs() < 1e-9 && !reset);
        assert_eq!(aggregate(&segs, 0, 500, Agg::Delta, "kWh"), Some(inc));
        assert_eq!(counter_increase(&segs, 3000, 4000), None);
    }

    #[test]
    fn missing_values_make_no_segment() {
        let samples = [(0, None), (100, Some(1.0))];
        let segs = build_segments(&samples, 0, 200, 200, &[]);
        assert_eq!(stat(&segs, 0, 200).covered_ms, 100);
    }
}
