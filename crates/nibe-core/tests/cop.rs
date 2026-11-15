use nibe_core::analysis::cop_table;
use nibe_core::api::ApiPoint;
use nibe_core::db::{Reader, Store};
use nibe_core::timeutil::{parse_tz, Bucket};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn point(id: i64, title: &str, unit: &str, divisor: i64) -> ApiPoint {
    serde_json::from_value(json!({
        "title": title, "description": "",
        "metadata": {"variableId": id, "variableType": "integer", "variableSize": "u32", "unit": unit,
                     "modbusRegisterType": "MODBUS_INPUT_REGISTER", "modbusRegisterID": 1, "isWritable": false, "divisor": divisor},
        "value": {"variableId": id, "isOk": true, "integerValue": 0}
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

const HOUR: i64 = 3_600_000;

/// Four hours of polls every minute. The tests query half an hour past the buckets they check:
/// a range's last bucket can't see a counter step exactly at its end, which the even test data
/// would otherwise put on every hour. the unit uses 500 W and the compressor 400 W, while the
/// energy log counts 1.8 + 0.2 kWh of heat per hour (1.6 + 0.2 of it from the compressor only).
/// The lifetime counters 28392/28393 are deliberately left out of the catalogue.
/// `comp_power(minute)` and `heating_raw(minute)` allow a gap or a reset to be injected.
fn record_three_hours(name: &str, comp_power: impl Fn(i64) -> Option<i64>, heating_raw: impl Fn(i64) -> i64) -> Reader {
    let path = tmp_db(name);
    let mut st = Store::open(&path).unwrap();
    st.set_meta("poll_interval_secs", "60").unwrap();
    let mut cat = BTreeMap::new();
    for (id, title, unit, div) in [
        (22130, "Instantaneous used power", "W", 1),
        (1801, "Compressor power input", "W", 1),
        (27378, "Heating, compressor only", "kWh", 10),
        (27379, "Hot water, compressor only", "kWh", 10),
        (27382, "Heating, including int. add. heat", "kWh", 10),
        (27383, "Hot water, including int. add. heat", "kWh", 10),
        (4, "Current outdoor temperature (BT1)", "°C", 10),
        (22, "Exhaust air (BT20)", "°C", 10),
        (1973, "Compressor status", "", 1),
    ] {
        cat.insert(id, point(id, title, unit, div));
    }
    st.upsert_catalog(&cat, 0).unwrap();
    for m in 0..=240 {
        // raw counters in 0.1 kWh, rising evenly; integer division gives the 0.1 kWh steps
        let values = [
            (22130, Some(500)),
            (1801, comp_power(m)),
            (27382, Some(heating_raw(m))),
            (27383, Some(5_000 + m * 2 / 60)),
            (27378, Some(9_000 + m * 16 / 60)),
            (27379, Some(4_500 + m * 2 / 60)),
            (4, Some(55)),
            (22, Some(210)),
            (1973, Some(1)),
        ];
        st.record(m * 60_000, &values).unwrap();
        st.log_poll(m * 60_000, "full", true, values.len(), 0, 5, None);
    }
    Reader::open(&path).unwrap()
}

fn col(v: &Value, name: &str) -> usize {
    v["columns"].as_array().unwrap().iter().position(|c| c == name).unwrap_or_else(|| panic!("no column {name}"))
}

fn num(row: &Value, i: usize) -> Option<f64> {
    row[i].as_f64()
}

fn approx(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 0.01)
}

#[test]
fn cop_from_energy_log_and_measured_power() {
    let r = record_three_hours("cop", |_| Some(400), |m| 10_000 + m * 18 / 60);
    let tz = parse_tz("Europe/Amsterdam").unwrap();
    let v = cop_table(&r, tz, 0, 3 * HOUR + HOUR / 2, Bucket::Fixed(HOUR)).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 4);
    for row in &rows[..3] {
        assert_eq!(row.as_array().unwrap().len(), v["columns"].as_array().unwrap().len());
        assert!(approx(num(row, col(&v, "coverage")), 1.0));
        assert!(approx(num(row, col(&v, "elec_unit_kwh")), 0.5));
        assert!(approx(num(row, col(&v, "elec_compressor_kwh")), 0.4));
        assert!(approx(num(row, col(&v, "heat_kwh")), 2.0));
        assert!(approx(num(row, col(&v, "heat_compressor_only_kwh")), 1.8));
        assert!(approx(num(row, col(&v, "heating_kwh")), 1.8));
        assert!(approx(num(row, col(&v, "hotwater_kwh")), 0.2));
        // 2.0 / 0.5 and 1.8 / 0.4: at 0.5 kWh per hour the electricity alone would not allow a COP
        assert!(approx(num(row, col(&v, "cop_unit")), 4.0), "{row}");
        assert!(approx(num(row, col(&v, "cop_compressor")), 4.5), "{row}");
        assert!(approx(num(row, col(&v, "outdoor_avg_c")), 5.5));
        assert!(approx(num(row, col(&v, "compressor_on_fraction")), 1.0));
        // informational counters missing from the catalogue: empty columns, not an error
        assert!(row[col(&v, "lifetime_produced_delta_kwh")].is_null());
        assert!(row[col(&v, "lifetime_consumed_delta_kwh")].is_null());
    }
    assert!(v["notes"].to_string().contains("28392/28393 are not in this pump's catalogue"));

    // 15 minutes give only about 0.5 kWh of heat: below the threshold, so no COP
    let v = cop_table(&r, tz, 0, HOUR + HOUR / 2, Bucket::Fixed(HOUR / 4)).unwrap();
    for row in &v["rows"].as_array().unwrap()[..4] {
        assert!(row[col(&v, "cop_unit")].is_null() && row[col(&v, "cop_compressor")].is_null(), "{row}");
        let heat = num(row, col(&v, "heat_kwh")).unwrap();
        assert!(heat > 0.3 && heat < 0.7, "{row}");
    }
}

#[test]
fn compressor_cop_needs_its_own_power_measured() {
    // the compressor reports no valid power for half of the second hour; the unit power is fine
    let r = record_three_hours("cop-gap", |m| if (60..90).contains(&m) { None } else { Some(400) }, |m| 10_000 + m * 18 / 60);
    let tz = parse_tz("Europe/Amsterdam").unwrap();
    let v = cop_table(&r, tz, HOUR, 2 * HOUR + HOUR / 2, Bucket::Fixed(HOUR)).unwrap();
    let row = &v["rows"][0];
    assert!(approx(num(row, col(&v, "cop_unit")), 4.0), "{row}");
    // 1.8 kWh of heat over only 0.2 kWh of measured electricity would give 9.0
    assert!(row[col(&v, "cop_compressor")].is_null(), "{row}");
    assert!(approx(num(row, col(&v, "coverage")), 0.5));
}

#[test]
fn energy_log_reset_is_not_negative_heat() {
    // the heating counter is cleared at minute 150 and counts up from zero again
    let r = record_three_hours("cop-reset", |_| Some(400), |m| if m < 150 { 10_000 + m * 18 / 60 } else { (m - 150) * 18 / 60 });
    let tz = parse_tz("Europe/Amsterdam").unwrap();
    let v = cop_table(&r, tz, 2 * HOUR, 3 * HOUR + HOUR / 2, Bucket::Fixed(HOUR)).unwrap();
    let row = &v["rows"][0];
    // 10036 -> 10044 before the reset (0.8 kWh), the drop to 0 adds nothing (the step that
    // fell on that poll is lost), then 0 -> 9 (0.9 kWh): 1.7 kWh instead of a drop of ~1000
    assert!(approx(num(row, col(&v, "heating_kwh")), 1.7), "{row}");
    assert!(approx(num(row, col(&v, "cop_unit")), 3.8), "{row}");
    assert!(v["notes"].to_string().contains("dropped"));
}
