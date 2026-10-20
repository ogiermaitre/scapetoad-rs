//! GeoJSON (RFC 7946) FeatureCollections.
//!
//! RFC 7946 mandates WGS 84 coordinates, but projected GeoJSON is common in practice, and a
//! cartogram is useless in lat/lon anyway. We write the coordinates we have, and add the
//! legacy `crs` member when the layer has an EPSG code so tools like QGIS place it correctly.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use geojson::{FeatureCollection, GeoJson, GeometryValue, JsonObject, JsonValue};

use super::{infer_schema, stem};
use crate::error::{Error, Result};
use crate::layer::{Crs, Feature, Layer, Value};

const FORMAT: &str = "geojson";

pub(super) fn read(path: &Path) -> Result<Layer> {
    let json: JsonValue = serde_json::from_reader(BufReader::new(File::open(path)?))
        .map_err(|e| Error::format(FORMAT, e))?;
    let crs = legacy_crs(&json);
    let gj: GeoJson = serde_json::from_value(json).map_err(|e| Error::format(FORMAT, e))?;
    let features = match gj {
        GeoJson::FeatureCollection(fc) => fc.features,
        GeoJson::Feature(f) => vec![f],
        GeoJson::Geometry(g) => vec![geojson::Feature {
            geometry: Some(g),
            ..Default::default()
        }],
    };

    let mut geoms = Vec::with_capacity(features.len());
    let mut rows = Vec::with_capacity(features.len());
    for f in features {
        let geometry = f
            .geometry
            .map(|g| geo_types::Geometry::<f64>::try_from(&g.value))
            .transpose()
            .map_err(|e| Error::format(FORMAT, e))?;
        geoms.push(geometry);
        rows.push(
            f.properties
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| (k, json_to_value(v)))
                .collect::<Vec<_>>(),
        );
    }
    let (fields, values) = infer_schema(&rows);
    Ok(Layer {
        name: stem(path),
        fields,
        features: geoms
            .into_iter()
            .zip(values)
            .map(|(geometry, values)| Feature { geometry, values })
            .collect(),
        crs: crs.or(Some(Crs {
            epsg: Some(4326),
            wkt: None,
        })),
    })
}

/// Reads the pre-RFC 7946 `"crs": {"type": "name", "properties": {"name": "EPSG:xxxx"}}` member.
fn legacy_crs(json: &JsonValue) -> Option<Crs> {
    let name = json.get("crs")?.get("properties")?.get("name")?.as_str()?;
    let code = name.rsplit([':', ',']).next()?.trim().parse::<i32>().ok()?;
    Some(Crs {
        epsg: Some(code),
        wkt: None,
    })
}

fn json_to_value(v: JsonValue) -> Value {
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

fn value_to_json(v: &Value) -> JsonValue {
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

pub(super) fn write(layer: &Layer, path: &Path) -> Result<()> {
    let features = layer
        .features
        .iter()
        .map(|f| {
            let properties: JsonObject = layer
                .fields
                .iter()
                .zip(&f.values)
                .map(|(field, v)| (field.name.clone(), value_to_json(v)))
                .collect();
            geojson::Feature {
                geometry: f
                    .geometry
                    .as_ref()
                    .map(|g| geojson::Geometry::new(GeometryValue::from(g))),
                properties: Some(properties),
                ..Default::default()
            }
        })
        .collect();
    let mut fc = FeatureCollection {
        features,
        ..Default::default()
    };
    if let Some(code) = layer
        .crs
        .as_ref()
        .and_then(|c| c.epsg)
        .filter(|&c| c != 4326)
    {
        let crs = serde_json::json!({
            "type": "name",
            "properties": { "name": format!("urn:ogc:def:crs:EPSG::{code}") }
        });
        fc.foreign_members
            .get_or_insert_with(Default::default)
            .insert("crs".to_string(), crs);
    }
    let mut out = BufWriter::new(File::create(path)?);
    serde_json::to_writer(&mut out, &fc).map_err(|e| Error::format(FORMAT, e))?;
    out.flush()?;
    Ok(())
}
