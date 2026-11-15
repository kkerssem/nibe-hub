//! Types for the local NIBE REST API (`/api/v1/devices/{n}/points`).

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
pub struct ApiPoint {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub metadata: ApiMeta,
    pub value: ApiValue,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiMeta {
    #[serde(rename = "variableId")]
    pub variable_id: i64,
    #[serde(rename = "variableType", default)]
    pub variable_type: String,
    #[serde(rename = "variableSize", default)]
    pub variable_size: String,
    #[serde(default)]
    pub unit: String,
    #[serde(rename = "modbusRegisterType", default)]
    pub modbus_register_type: String,
    #[serde(rename = "modbusRegisterID", default)]
    pub modbus_register_id: Option<i64>,
    #[serde(rename = "isWritable", default)]
    pub is_writable: bool,
    #[serde(default = "one")]
    pub divisor: i64,
}

fn one() -> i64 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct ApiValue {
    #[serde(rename = "variableId", default)]
    pub variable_id: i64,
    #[serde(rename = "isOk", default = "yes")]
    pub is_ok: bool,
    #[serde(rename = "integerValue", default)]
    pub integer_value: i64,
}

fn yes() -> bool {
    true
}

impl ApiPoint {
    /// The raw value, or `None` if the sensor is missing or the value is not valid.
    pub fn raw(&self) -> Option<i64> {
        normalize_raw(&self.metadata.variable_size, self.value.is_ok, self.value.integer_value)
    }
}

/// NIBE returns -32768 (s16) for a sensor that is not connected.
/// (255 in a u8 is an ordinary value, e.g. "not configured", and is kept.)
pub fn normalize_raw(size: &str, is_ok: bool, v: i64) -> Option<i64> {
    if !is_ok {
        return None;
    }
    if size == "s16" && v == -32768 {
        return None;
    }
    Some(v)
}

/// Parses the response of `GET /points` (map id -> point).
pub fn parse_points(body: &[u8]) -> Result<BTreeMap<i64, ApiPoint>> {
    let map: BTreeMap<String, ApiPoint> = serde_json::from_slice(body)?;
    let mut out = BTreeMap::new();
    for (k, p) in map {
        let id: i64 = k.parse().map_err(|_| anyhow!("invalid point id {k:?}"))?;
        out.insert(id, p);
    }
    Ok(out)
}

/// Parses the response of `GET /points/{id}`. Accepts a single point, a list or a map,
/// so an unexpected shape of the firmware response is not immediately fatal.
pub fn parse_point_response(body: &[u8]) -> Result<Vec<ApiPoint>> {
    let v: Value = serde_json::from_slice(body)?;
    let mut out = Vec::new();
    match v {
        Value::Array(items) => {
            for it in items {
                out.push(serde_json::from_value(it)?);
            }
        }
        Value::Object(ref m) if m.contains_key("metadata") && m.contains_key("value") => {
            out.push(serde_json::from_value(v)?);
        }
        Value::Object(m) => {
            for (_, it) in m {
                out.push(serde_json::from_value(it)?);
            }
        }
        _ => return Err(anyhow!("unexpected response to point request")),
    }
    Ok(out)
}

/// Removes the soft hyphens (U+00AD) that the API includes in titles.
pub fn clean_title(s: &str) -> String {
    s.chars().filter(|c| *c != '\u{00ad}').collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim response of `GET /api/v1/devices/0/points/1801` from an S735C-7 (firmware 4.6.x).
    const POINT_1801: &str = r#"{"title": "Compres­sor power input", "description": "", "metadata": {"type": "metadata", "variableId": 1801, "variableType": "integer", "variableSize": "u16", "unit": "W", "modbusRegisterType": "MODBUS_INPUT_REGISTER", "shortUnit": "W", "isWritable": false, "divisor": 1, "decimal": 0, "modbusRegisterID": 1048, "minValue": 0, "maxValue": 0, "intDefaultValue": 0, "change": 202, "stringDefaultValue": ""}, "value": {"type": "datavalue", "isOk": true, "variableId": 1801, "integerValue": 372, "stringValue": ""}}"#;

    #[test]
    fn single_point_response_from_the_pump() {
        let pts = parse_point_response(POINT_1801.as_bytes()).unwrap();
        assert_eq!(pts.len(), 1);
        let p = &pts[0];
        assert_eq!((p.metadata.variable_id, p.raw()), (1801, Some(372)));
        assert_eq!(clean_title(&p.title), "Compressor power input");
    }

    #[test]
    fn not_found_response_is_an_error() {
        // the pump answers 404 with this body; `get` fails on the status first, but the parser must not accept it either
        assert!(parse_point_response(br#"{"error": "Point not found"}"#).is_err());
    }
}
