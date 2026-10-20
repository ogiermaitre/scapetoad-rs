//! Any vector format GDAL supports (GeoPackage, GeoParquet, GML, KML, …), behind the
//! `gdal` feature. Requires libgdal at build time.

use std::path::Path;

use gdal::spatial_ref::SpatialRef;
use gdal::vector::{
    FieldDefn, FieldValue, LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType, ToGdal,
};
use gdal::{Dataset, DatasetOptions, DriverManager, GdalOpenFlags};

use super::stem;
use crate::error::{Error, Result};
use crate::layer::{Crs, Feature, Field, FieldType, Layer, Value};

const FORMAT: &str = "gdal";

fn err(e: impl std::fmt::Display) -> Error {
    Error::format(FORMAT, e)
}

/// GDAL driver for an output extension. Reading needs no mapping: GDAL probes the file.
fn driver_name(path: &Path) -> Result<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    Ok(match ext.as_str() {
        "gpkg" => "GPKG",
        "parquet" | "geoparquet" => "Parquet",
        "arrow" | "feather" | "arrows" | "ipc" => "Arrow",
        "sqlite" | "db" => "SQLite",
        "gml" => "GML",
        "kml" => "KML",
        "csv" => "CSV",
        "gdb" => "OpenFileGDB",
        "geojsonl" | "geojsons" | "geojsonseq" => "GeoJSONSeq",
        "shp" => "ESRI Shapefile",
        "geojson" | "json" => "GeoJSON",
        "fgb" => "FlatGeobuf",
        _ => return Err(Error::UnsupportedFormat(ext)),
    })
}

/// Formats that hold several layers: writing adds or replaces one layer instead of
/// replacing the whole file.
fn multi_layer(driver: &str) -> bool {
    matches!(driver, "GPKG" | "SQLite" | "OpenFileGDB")
}

pub(super) fn read(path: &Path, layer_name: Option<&str>) -> Result<Layer> {
    let ds = Dataset::open_ex(
        path,
        DatasetOptions {
            open_flags: GdalOpenFlags::GDAL_OF_VECTOR | GdalOpenFlags::GDAL_OF_READONLY,
            ..Default::default()
        },
    )
    .map_err(err)?;
    let mut lyr = match layer_name {
        Some(name) => ds.layer_by_name(name).map_err(err)?,
        None => {
            if ds.layer_count() > 1 {
                let names: Vec<String> = ds.layers().map(|l| l.name()).collect();
                return Err(Error::Format {
                    format: FORMAT,
                    message: format!(
                        "{} has {} layers ({}); pick one with {}#<layer>",
                        path.display(),
                        names.len(),
                        names.join(", "),
                        path.display()
                    ),
                });
            }
            ds.layer(0).map_err(err)?
        }
    };

    let fields: Vec<Field> = lyr
        .defn()
        .fields()
        .map(|f| {
            let kind = match f.field_type() {
                OGRFieldType::OFTInteger | OGRFieldType::OFTInteger64 => FieldType::Int,
                OGRFieldType::OFTReal => FieldType::Float,
                _ => FieldType::Text,
            };
            Field::new(f.name(), kind)
        })
        .collect();
    let crs = lyr.spatial_ref().map(|srs| Crs {
        epsg: (srs.auth_name().as_deref() == Some("EPSG"))
            .then(|| srs.auth_code().ok())
            .flatten(),
        wkt: srs.to_wkt().ok(),
    });
    let name = lyr.name();

    let mut features = Vec::new();
    for feat in lyr.features() {
        let geometry = feat
            .geometry()
            .map(|g| g.to_geo())
            .transpose()
            .map_err(err)?;
        let values = (0..fields.len())
            .map(|i| {
                Ok(match feat.field(i).map_err(err)? {
                    None => Value::Null,
                    Some(FieldValue::IntegerValue(x)) => Value::Int(i64::from(x)),
                    Some(FieldValue::Integer64Value(x)) => Value::Int(x),
                    Some(FieldValue::RealValue(x)) => Value::Float(x),
                    Some(FieldValue::StringValue(s)) => Value::Text(s),
                    Some(FieldValue::DateValue(d)) => Value::Text(d.to_string()),
                    Some(FieldValue::DateTimeValue(d)) => Value::Text(d.to_rfc3339()),
                    Some(other) => Value::Text(format!("{other:?}")),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        features.push(Feature { geometry, values });
    }

    Ok(Layer {
        name: if name.is_empty() { stem(path) } else { name },
        fields,
        features,
        crs,
    })
}

pub(super) fn write(layer: &Layer, path: &Path, layer_name: Option<&str>) -> Result<()> {
    let driver_name = driver_name(path)?;
    let driver = DriverManager::get_driver_by_name(driver_name).map_err(err)?;
    let mut ds = if path.exists() && multi_layer(driver_name) {
        Dataset::open_ex(
            path,
            DatasetOptions {
                open_flags: GdalOpenFlags::GDAL_OF_VECTOR | GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )
        .map_err(err)?
    } else {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        driver.create_vector_only(path).map_err(err)?
    };

    let srs = match &layer.crs {
        Some(Crs { wkt: Some(wkt), .. }) => SpatialRef::from_wkt(wkt).ok(),
        Some(Crs {
            epsg: Some(code), ..
        }) => u32::try_from(*code)
            .ok()
            .and_then(|c| SpatialRef::from_epsg(c).ok()),
        _ => None,
    };
    let name = layer_name.unwrap_or(&layer.name);
    // Database formats are very slow without a transaction; file formats don't support one.
    if multi_layer(driver_name) {
        let mut txn = ds.start_transaction().map_err(err)?;
        fill(&mut txn, layer, name, srs.as_ref(), true)?;
        txn.commit().map_err(err)
    } else {
        fill(&mut ds, layer, name, srs.as_ref(), false)
    }
}

fn fill(
    ds: &mut Dataset,
    layer: &Layer,
    name: &str,
    srs: Option<&SpatialRef>,
    overwrite: bool,
) -> Result<()> {
    let overwrite_opt = ["OVERWRITE=YES"];
    {
        let lyr = ds
            .create_layer(LayerOptions {
                name,
                srs,
                ty: OGRwkbGeometryType::wkbUnknown,
                options: overwrite.then_some(&overwrite_opt[..]),
            })
            .map_err(err)?;
        for field in &layer.fields {
            let ty = match field.kind {
                FieldType::Bool | FieldType::Int => OGRFieldType::OFTInteger64,
                FieldType::Float => OGRFieldType::OFTReal,
                FieldType::Text => OGRFieldType::OFTString,
            };
            FieldDefn::new(&field.name, ty)
                .and_then(|d| d.add_to_layer(&lyr))
                .map_err(err)?;
        }
        for feat in &layer.features {
            let mut out = gdal::vector::Feature::new(lyr.defn()).map_err(err)?;
            if let Some(g) = &feat.geometry {
                out.set_geometry(g.to_gdal().map_err(err)?).map_err(err)?;
            }
            for (i, (field, v)) in layer.fields.iter().zip(&feat.values).enumerate() {
                let res = match (field.kind, v) {
                    (_, Value::Null) => out.set_field_null(i),
                    (FieldType::Bool, Value::Bool(b)) => out.set_field_integer64(i, i64::from(*b)),
                    (FieldType::Int, v) => match v.as_f64() {
                        Some(x) => out.set_field_integer64(i, x.round() as i64),
                        None => out.set_field_null(i),
                    },
                    (FieldType::Float, v) => match v.as_f64() {
                        Some(x) => out.set_field_double(i, x),
                        None => out.set_field_null(i),
                    },
                    (_, v) => out.set_field_string(i, &v.to_string()),
                };
                res.map_err(err)?;
            }
            out.create(&lyr).map_err(err)?;
        }
    }
    Ok(())
}
