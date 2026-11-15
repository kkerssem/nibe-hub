use nibe_core::analysis::series_table;
use nibe_core::api::ApiPoint;
use nibe_core::db::{Reader, Store};
use nibe_core::series::Agg;
use nibe_core::timeutil::{parse_tz, Bucket};
use serde_json::json;
use std::collections::BTreeMap;

fn point(id: i64, title: &str, unit: &str, divisor: i64, size: &str, raw: i64) -> ApiPoint {
    serde_json::from_value(json!({
        "title": title, "description": "",
        "metadata": {"variableId": id, "variableType": "integer", "variableSize": size, "unit": unit,
                     "modbusRegisterType": "MODBUS_INPUT_REGISTER", "modbusRegisterID": 1, "isWritable": false, "divisor": divisor},
        "value": {"variableId": id, "isOk": true, "integerValue": raw}
    }))
    .unwrap()
}

fn tmp_db(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("nibe-test-{}-{name}.db", std::process::id()));
    for ext in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", p.display()));
    }
    p
}

#[test]
fn collector_outage_is_a_gap_not_a_flat_line() {
    let path = tmp_db("gap");
    let mut st = Store::open(&path).unwrap();
    st.set_meta("poll_interval_secs", "60").unwrap();
    let mut cat = BTreeMap::new();
    cat.insert(1, point(1, "Test power", "W", 1, "u16", 100));
    cat.insert(2, point(2, "Sensor", "°C", 10, "s16", 215));
    st.upsert_catalog(&cat, 0).unwrap();

    // polls every minute, then nothing for 8 minutes, then polls again
    for t in [0, 60, 120] {
        st.record(t * 1000, &[(1, Some(100)), (2, Some(215))]).unwrap();
        st.log_poll(t * 1000, "full", true, 2, 0, 5, None);
    }
    for t in [600, 660] {
        st.record(t * 1000, &[(1, Some(200)), (2, Some(215))]).unwrap();
        st.log_poll(t * 1000, "full", true, 2, 0, 5, None);
    }
    // only changes are stored: 2 initial values + 1 change of point 1
    assert_eq!(Reader::open(&path).unwrap().stats().unwrap().samples, 3);

    let r = Reader::open(&path).unwrap();
    let tz = parse_tz("Europe/Amsterdam").unwrap();
    let v = series_table(&r, tz, &[1], 0, 720_000, Bucket::Fixed(720_000), &[Agg::Avg, Agg::Last]).unwrap();
    let row = &v["rows"][0];
    // measured time: 0..180 s and 600..720 s = 300 s of 720 s
    let coverage = row[1].as_f64().unwrap();
    assert!((coverage - 0.42).abs() < 0.01, "coverage {coverage}");
    // (100 W × 180 s + 200 W × 120 s) / 300 s = 140 W
    assert_eq!(row[2].as_f64().unwrap(), 140.0);
    assert_eq!(row[3].as_f64().unwrap(), 200.0);

    // scale: 215 / 10 = 21.5 °C
    let v = series_table(&r, tz, &[2], 0, 720_000, Bucket::Fixed(720_000), &[Agg::Avg]).unwrap();
    assert_eq!(v["rows"][0][2].as_f64().unwrap(), 21.5);
}

#[test]
fn history_older_than_the_prune_horizon_stays_queryable() {
    let path = tmp_db("prune");
    let mut st = Store::open(&path).unwrap();
    st.set_meta("poll_interval_secs", "60").unwrap();
    let mut cat = BTreeMap::new();
    cat.insert(1, point(1, "Test power", "W", 1, "u16", 100));
    st.upsert_catalog(&cat, 0).unwrap();
    for t in [0, 60, 120, 180] {
        st.record(t * 1000, &[(1, Some(100))]).unwrap();
        st.log_poll(t * 1000, "full", true, 1, 0, 5, None);
    }
    st.log_poll(30_000, "full", false, 0, 0, 5, Some("timeout"));
    st.log_poll(90_000, "fast", false, 0, 0, 5, Some("timeout"));
    // prune everything "older than 90 days": only the failed and fast entries may go
    assert_eq!(st.prune_polls(i64::MAX).unwrap(), 2);

    let r = Reader::open(&path).unwrap();
    let tz = parse_tz("Europe/Amsterdam").unwrap();
    let v = series_table(&r, tz, &[1], 0, 240_000, Bucket::Fixed(240_000), &[Agg::Avg]).unwrap();
    assert_eq!(v["rows"][0][2].as_f64().unwrap(), 100.0);
    assert_eq!(v["rows"][0][1].as_f64().unwrap(), 1.0);
}

#[test]
fn an_older_value_never_overwrites_a_newer_one() {
    let path = tmp_db("order");
    let mut st = Store::open(&path).unwrap();
    // a fast poll stored X=5 at t=2 s; the slower full poll's X=6 arrives stamped t=1 s
    assert_eq!(st.record(2000, &[(7, Some(5))]).unwrap(), 1);
    assert_eq!(st.record(1000, &[(7, Some(6))]).unwrap(), 0);
    assert_eq!(st.record(2000, &[(7, Some(6))]).unwrap(), 0);
    assert_eq!(st.record(3000, &[(7, Some(6))]).unwrap(), 1);
    let r = Reader::open(&path).unwrap();
    assert_eq!(r.latest(7).unwrap(), Some((3000, Some(6))));
}

#[test]
fn a_failed_write_is_retried_on_the_next_poll() {
    let path = tmp_db("retry");
    let mut st = Store::open(&path).unwrap();
    st.record(1000, &[(1, Some(10))]).unwrap();
    // make the next insert fail, as a full disk or an I/O error would
    let other = rusqlite::Connection::open(&path).unwrap();
    other.execute_batch("ALTER TABLE samples RENAME TO samples_away").unwrap();
    assert!(st.record(2000, &[(1, Some(11))]).is_err());
    other.execute_batch("ALTER TABLE samples_away RENAME TO samples").unwrap();
    // the change was not stored, so the next poll with the same value must store it
    assert_eq!(st.record(3000, &[(1, Some(11))]).unwrap(), 1);
    assert_eq!(Reader::open(&path).unwrap().latest(1).unwrap(), Some((3000, Some(11))));
}

#[test]
fn stats_come_from_counters_and_survive_a_reopen() {
    let path = tmp_db("stats");
    {
        let mut st = Store::open(&path).unwrap();
        st.record(1000, &[(1, Some(1)), (2, Some(2))]).unwrap();
        st.record(5000, &[(1, Some(3)), (2, Some(2))]).unwrap();
    }
    let s = Reader::open(&path).unwrap().stats().unwrap();
    assert_eq!((s.samples, s.first_sample_ms, s.last_sample_ms), (3, Some(1000), Some(5000)));

    // a database from before the counters existed gets them computed once on open
    rusqlite::Connection::open(&path).unwrap().execute_batch("DELETE FROM meta").unwrap();
    let mut st = Store::open(&path).unwrap();
    st.record(9000, &[(1, Some(4))]).unwrap();
    let s = Reader::open(&path).unwrap().stats().unwrap();
    assert_eq!((s.samples, s.first_sample_ms, s.last_sample_ms), (4, Some(1000), Some(9000)));
}

#[test]
fn raw_series_shares_the_row_budget_per_point() {
    let path = tmp_db("raw");
    let mut st = Store::open(&path).unwrap();
    let mut cat = BTreeMap::new();
    cat.insert(1, point(1, "A", "W", 1, "u16", 0));
    cat.insert(2, point(2, "B", "W", 1, "u16", 0));
    st.upsert_catalog(&cat, 0).unwrap();
    for i in 0..1500i64 {
        st.record(i * 1000, &[(1, Some(i)), (2, Some(-i))]).unwrap();
    }
    let r = Reader::open(&path).unwrap();
    let tz = parse_tz("Europe/Amsterdam").unwrap();
    let v = series_table(&r, tz, &[1, 2], 0, 1_500_000, Bucket::Raw, &[]).unwrap();
    let rows = v["rows"].as_array().unwrap();
    let count = |id: i64| rows.iter().filter(|r| r[1].as_i64() == Some(id)).count();
    assert_eq!((count(1), count(2)), (1000, 1000));
    assert!(v["notes"][0].as_str().unwrap().contains("truncated"));
}

#[test]
fn sentinel_means_no_sensor() {
    use nibe_core::api::normalize_raw;
    assert_eq!(normalize_raw("s16", true, -32768), None);
    assert_eq!(normalize_raw("u8", true, 255), Some(255));
    assert_eq!(normalize_raw("s16", false, 100), None);
}
