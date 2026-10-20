//! Helpers shared by the JSON formats (GeoJSON, TopoJSON).

use serde_json::Value as JsonValue;

use crate::layer::{Crs, Value};

/// Reads the pre-RFC 7946 `"crs": {"type": "name", "properties": {"name": "EPSG:xxxx"}}` member.
pub(super) fn legacy_crs(json: &JsonValue) -> Option<Crs> {
    let name = json.get("crs")?.get("properties")?.get("name")?.as_str()?;
    let code = name.rsplit([':', ',']).next()?.trim().parse::<i32>().ok()?;
    Some(Crs {
        epsg: Some(code),
        wkt: None,
    })
}

/// The `crs` member for a projected layer, as QGIS and GDAL read it; `None` for WGS 84 or
/// an unknown CRS.
pub(super) fn crs_member(crs: Option<&Crs>) -> Option<JsonValue> {
    let code = crs.and_then(|c| c.epsg).filter(|&c| c != 4326)?;
    Some(serde_json::json!({
        "type": "name",
        "properties": { "name": format!("urn:ogc:def:crs:EPSG::{code}") }
    }))
}

pub(super) fn json_to_value(v: JsonValue) -> Value {
    match v {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(b) => Value::Bool(b),
        JsonValue::Number(n) => n
            .as_i64()
            .map(Value::Int)
            .or_else(|| n.as_f64().map(Value::Float))
            .unwrap_or(Value::Null),
        JsonValue::String(s) => Value::Text(s),
        other => Value::Text(other.to_string()),
    }
}

pub(super) fn value_to_json(v: &Value) -> JsonValue {
    match v {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Int(i) => JsonValue::from(*i),
        Value::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(JsonValue::Null, JsonValue::Number)
        }
        Value::Text(s) => JsonValue::String(s.clone()),
    }
}
