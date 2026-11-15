//! Typen voor de lokale NIBE REST-API (`/api/v1/devices/{n}/points`).

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
    /// De ruwe waarde, of `None` als de sensor ontbreekt of de waarde niet geldig is.
    pub fn raw(&self) -> Option<i64> {
        normalize_raw(&self.metadata.variable_size, self.value.is_ok, self.value.integer_value)
    }
}

/// NIBE geeft -32768 (s16) terug voor een sensor die niet is aangesloten.
/// (255 bij een u8 is een gewone waarde, bv. "niet geconfigureerd", en blijft staan.)
pub fn normalize_raw(size: &str, is_ok: bool, v: i64) -> Option<i64> {
    if !is_ok {
        return None;
    }
    if size == "s16" && v == -32768 {
        return None;
    }
    Some(v)
}

/// Parseert het antwoord van `GET /points` (map id -> punt).
pub fn parse_points(body: &[u8]) -> Result<BTreeMap<i64, ApiPoint>> {
    let map: BTreeMap<String, ApiPoint> = serde_json::from_slice(body)?;
    let mut out = BTreeMap::new();
    for (k, p) in map {
        let id: i64 = k.parse().map_err(|_| anyhow!("ongeldige punt-id {k:?}"))?;
        out.insert(id, p);
    }
    Ok(out)
}

/// Parseert het antwoord van `GET /points/{id}`. Accepteert één punt, een lijst of een map,
/// zodat een afwijkende vorm van het firmware-antwoord niet meteen fataal is.
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
        _ => return Err(anyhow!("onverwacht antwoord voor puntaanvraag")),
    }
    Ok(out)
}

/// Haalt de zachte afbreekstreepjes (U+00AD) uit titels, zoals de API ze meelevert.
pub fn clean_title(s: &str) -> String {
    s.chars().filter(|c| *c != '\u{00ad}').collect()
}
