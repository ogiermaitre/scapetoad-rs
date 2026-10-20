//! GeoJSON (RFC 7946) FeatureCollections.
//!
//! RFC 7946 mandates WGS 84 coordinates, but projected GeoJSON is common in practice, and a
//! cartogram is useless in lat/lon anyway. We write the coordinates we have, and add the
//! legacy `crs` member when the layer has an EPSG code so tools like QGIS place it correctly.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use geojson::{FeatureCollection, GeoJson, GeometryValue, JsonObject, JsonValue};

use super::json::{crs_member, json_to_value, legacy_crs, value_to_json};
use super::{infer_schema, stem};
use crate::error::{Error, Result};
use crate::layer::{Crs, Feature, Layer};

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
    if let Some(crs) = crs_member(layer.crs.as_ref()) {
        fc.foreign_members
            .get_or_insert_with(Default::default)
            .insert("crs".to_string(), crs);
    }
    let mut out = BufWriter::new(File::create(path)?);
    serde_json::to_writer(&mut out, &fc).map_err(|e| Error::format(FORMAT, e))?;
    out.flush()?;
    Ok(())
}
