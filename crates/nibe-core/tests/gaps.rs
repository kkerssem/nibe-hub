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
fn sentinel_means_no_sensor() {
    use nibe_core::api::normalize_raw;
    assert_eq!(normalize_raw("s16", true, -32768), None);
    assert_eq!(normalize_raw("u8", true, 255), Some(255));
    assert_eq!(normalize_raw("s16", false, 100), None);
}
