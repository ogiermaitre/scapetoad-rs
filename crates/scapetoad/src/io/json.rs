//! Helpers shared by the JSON formats (GeoJSON, TopoJSON).

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use serde_json::Value as JsonValue;

use crate::error::{Error, Result};
use crate::layer::{Crs, Layer, Value};

/// Reads a GeoJSON or TopoJSON file, whatever its extension says: TopoJSON is often
/// published as `.json`, so the document's `"type"` decides. `object` picks a TopoJSON object.
pub(super) fn read(path: &Path, object: Option<&str>) -> Result<Layer> {
    let root: JsonValue = serde_json::from_reader(BufReader::new(File::open(path)?))
        .map_err(|e| Error::format("json", e))?;
    let topology = root.get("type").and_then(JsonValue::as_str) == Some("Topology");
    if topology {
        #[cfg(feature = "topojson")]
        return super::topojson::from_json(root, path, object);
        #[cfg(not(feature = "topojson"))]
        return Err(Error::UnsupportedFormat(
            "TopoJSON (build with the `topojson` feature)".into(),
        ));
    }
    let _ = object;
    #[cfg(feature = "geojson")]
    return super::geojson::from_json(root, path);
    #[cfg(not(feature = "geojson"))]
    Err(Error::UnsupportedFormat(
        "GeoJSON (build with the `geojson` feature)".into(),
    ))
}

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
