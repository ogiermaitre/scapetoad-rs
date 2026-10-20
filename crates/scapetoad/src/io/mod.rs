//! Reading and writing layers. The format is chosen from the file extension, except that a
//! JSON file is read as TopoJSON or GeoJSON according to its content.
//!
//! Built in (pure Rust, default features): Shapefile (`.shp`), GeoJSON (`.geojson`, `.json`),
//! FlatGeobuf (`.fgb`), TopoJSON (`.topojson`, or `.json`). With the `gdal` feature, every
//! other extension goes through GDAL (GeoPackage, GeoParquet, …). Multi-layer sources take a
//! layer name after `#`: `data.gpkg#roads`, `map.topojson#communes`.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::layer::{Field, FieldType, Layer, Value};

#[cfg(feature = "fgb")]
mod fgb;
#[cfg(feature = "gdal")]
mod gdal;
#[cfg(feature = "geojson")]
mod geojson;
#[cfg(any(feature = "geojson", feature = "topojson"))]
mod json;
#[cfg(feature = "shp")]
mod shp;
#[cfg(feature = "topojson")]
mod topojson;

/// File formats known to this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Shapefile,
    GeoJson,
    FlatGeobuf,
    /// Several layers sharing their borders; written whole, see [`write_layers`].
    TopoJson,
    /// Anything else, handled by GDAL when the `gdal` feature is enabled.
    Gdal,
}

/// Options for formats that have them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// TopoJSON: round positions to an `n` × `n` grid over the extent and delta-encode them,
    /// which makes files much smaller (`Some(1_000_000)` is about 0.35 m over Switzerland).
    /// `None` keeps exact coordinates.
    pub topojson_quantization: Option<u32>,
}

impl Format {
    /// Format for a path, from its extension.
    pub fn from_path(path: &Path) -> Format {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("shp") => Format::Shapefile,
            Some("geojson" | "json") => Format::GeoJson,
            Some("fgb") => Format::FlatGeobuf,
            Some("topojson") => Format::TopoJson,
            _ => Format::Gdal,
        }
    }
}

/// Splits `file.gpkg#layer` into the path and the layer name. A `#` that is part of an
/// existing file name is kept.
fn split_source(source: &Path) -> (PathBuf, Option<String>) {
    if source.exists() {
        return (source.to_path_buf(), None);
    }
    let s = source.to_string_lossy();
    match s.rsplit_once('#') {
        Some((path, layer)) if !layer.is_empty() => (PathBuf::from(path), Some(layer.to_string())),
        _ => (source.to_path_buf(), None),
    }
}

/// Reads a layer. See the module docs for supported formats and the `#layer` syntax.
pub fn read_layer(source: impl AsRef<Path>) -> Result<Layer> {
    let (path, layer_name) = split_source(source.as_ref());
    let mut layer = match Format::from_path(&path) {
        #[cfg(feature = "shp")]
        Format::Shapefile => shp::read(&path)?,
        #[cfg(any(feature = "geojson", feature = "topojson"))]
        Format::GeoJson | Format::TopoJson => json::read(&path, layer_name.as_deref())?,
        #[cfg(feature = "fgb")]
        Format::FlatGeobuf => fgb::read(&path)?,
        #[cfg(feature = "gdal")]
        _ => return gdal::read(&path, layer_name.as_deref()),
        #[allow(unreachable_patterns)]
        _ => return Err(unsupported(&path)),
    };
    if let Some(name) = layer_name {
        layer.name = name;
    }
    Ok(layer)
}

/// Splits a write target `file.gpkg#layer` into the path and the layer name.
fn split_target(target: &Path) -> (PathBuf, Option<String>) {
    match target.to_string_lossy().rsplit_once('#') {
        Some((p, l)) if !l.is_empty() => (PathBuf::from(p), Some(l.to_string())),
        _ => (target.to_path_buf(), None),
    }
}

/// Writes a layer, replacing an existing file. For multi-layer formats, the layer is named
/// after `#` or else after [`Layer::name`]. A GeoPackage gains or replaces that layer; a
/// TopoJSON file is replaced by one holding only this layer (use [`write_layers`] for several).
pub fn write_layer(layer: &Layer, target: impl AsRef<Path>) -> Result<()> {
    let (path, layer_name) = split_target(target.as_ref());
    match Format::from_path(&path) {
        #[cfg(feature = "topojson")]
        Format::TopoJson => {
            let named;
            let layer = match layer_name {
                Some(name) => {
                    named = Layer {
                        name,
                        ..layer.clone()
                    };
                    &named
                }
                None => layer,
            };
            topojson::write(&[layer], &path, None)
        }
        #[cfg(feature = "shp")]
        Format::Shapefile => shp::write(layer, &path),
        #[cfg(feature = "geojson")]
        Format::GeoJson => geojson::write(layer, &path),
        #[cfg(feature = "fgb")]
        Format::FlatGeobuf => fgb::write(layer, &path),
        #[cfg(feature = "gdal")]
        _ => gdal::write(layer, &path, layer_name.as_deref()),
        #[allow(unreachable_patterns)]
        _ => {
            let _ = layer_name;
            Err(unsupported(&path))
        }
    }
}

/// Writes several layers into one file, each named after [`Layer::name`]. In TopoJSON they
/// become objects of one topology, so borders shared across layers (communes and cantons)
/// are stored once. GDAL formats get one layer each; single-layer formats take exactly one.
pub fn write_layers(
    layers: &[&Layer],
    target: impl AsRef<Path>,
    options: &WriteOptions,
) -> Result<()> {
    let path = target.as_ref();
    match Format::from_path(path) {
        #[cfg(feature = "topojson")]
        Format::TopoJson => topojson::write(layers, path, options.topojson_quantization),
        #[cfg(feature = "gdal")]
        Format::Gdal => layers.iter().try_for_each(|layer| {
            let target = PathBuf::from(format!("{}#{}", path.display(), layer.name));
            write_layer(layer, target)
        }),
        _ => match layers {
            [layer] => {
                let _ = options;
                write_layer(layer, path)
            }
            _ => Err(Error::format(
                "write",
                format!(
                    "{} holds a single layer, got {}",
                    path.display(),
                    layers.len()
                ),
            )),
        },
    }
}

/// Default layer name for single-layer files.
#[allow(dead_code)]
pub(crate) fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[allow(dead_code)]
fn unsupported(path: &Path) -> Error {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    Error::UnsupportedFormat(if cfg!(feature = "gdal") {
        ext
    } else {
        format!(
            "{ext} (build with the `gdal` feature for GeoPackage, GeoParquet and other formats)"
        )
    })
}

/// Builds a schema from per-feature `(name, value)` lists for formats without a fixed
/// schema (GeoJSON). Column order follows first appearance; a column holding both integers
/// and floats becomes `Float`, any other mix becomes `Text`.
#[allow(dead_code)]
pub(crate) fn infer_schema(rows: &[Vec<(String, Value)>]) -> (Vec<Field>, Vec<Vec<Value>>) {
    let mut fields: Vec<Field> = Vec::new();
    let mut kinds: Vec<Option<FieldType>> = Vec::new();
    for row in rows {
        for (name, value) in row {
            let idx = match fields.iter().position(|f| &f.name == name) {
                Some(i) => i,
                None => {
                    fields.push(Field::new(name.clone(), FieldType::Text));
                    kinds.push(None);
                    fields.len() - 1
                }
            };
            let kind = match value {
                Value::Null => continue,
                Value::Bool(_) => FieldType::Bool,
                Value::Int(_) => FieldType::Int,
                Value::Float(_) => FieldType::Float,
                Value::Text(_) => FieldType::Text,
            };
            kinds[idx] = Some(match (kinds[idx], kind) {
                (None, k) => k,
                (Some(a), b) if a == b => a,
                (Some(FieldType::Int | FieldType::Float), FieldType::Int | FieldType::Float) => {
                    FieldType::Float
                }
                _ => FieldType::Text,
            });
        }
    }
    for (field, kind) in fields.iter_mut().zip(&kinds) {
        field.kind = kind.unwrap_or(FieldType::Text);
    }
    let values = rows
        .iter()
        .map(|row| {
            fields
                .iter()
                .map(|f| {
                    let v = row
                        .iter()
                        .find(|(n, _)| n == &f.name)
                        .map_or(Value::Null, |(_, v)| v.clone());
                    coerce(v, f.kind)
                })
                .collect()
        })
        .collect();
    (fields, values)
}

/// Converts a value to the column type, so every value in a column has the same variant.
#[allow(dead_code)]
pub(crate) fn coerce(v: Value, kind: FieldType) -> Value {
    match (v, kind) {
        (Value::Null, _) => Value::Null,
        (Value::Int(i), FieldType::Float) => Value::Float(i as f64),
        (v @ (Value::Bool(_) | Value::Int(_) | Value::Float(_)), FieldType::Text) => {
            Value::Text(v.to_string())
        }
        (v, _) => v,
    }
}
