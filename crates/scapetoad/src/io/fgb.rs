//! FlatGeobuf (`.fgb`).

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use flatgeobuf::{
    ColumnType, FallibleStreamingIterator, FgbCrs, FgbReader, FgbWriter, FgbWriterOptions,
    GeometryType,
};
use geo_types::Geometry;
use geozero::{ColumnValue, FeatureProperties, PropertyProcessor, ToGeo};

use super::stem;
use crate::error::{Error, Result};
use crate::layer::{Crs, Feature, Field, FieldType, Layer, Value};

const FORMAT: &str = "flatgeobuf";

fn err(e: impl std::fmt::Display) -> Error {
    Error::format(FORMAT, e)
}

/// Collects the properties of one feature by column index.
struct Row(Vec<Value>);

impl PropertyProcessor for Row {
    fn property(&mut self, i: usize, _name: &str, v: &ColumnValue) -> geozero::error::Result<bool> {
        if let Some(slot) = self.0.get_mut(i) {
            *slot = match v {
                ColumnValue::Bool(b) => Value::Bool(*b),
                ColumnValue::Byte(x) => Value::Int(i64::from(*x)),
                ColumnValue::UByte(x) => Value::Int(i64::from(*x)),
                ColumnValue::Short(x) => Value::Int(i64::from(*x)),
                ColumnValue::UShort(x) => Value::Int(i64::from(*x)),
                ColumnValue::Int(x) => Value::Int(i64::from(*x)),
                ColumnValue::UInt(x) => Value::Int(i64::from(*x)),
                ColumnValue::Long(x) => Value::Int(*x),
                ColumnValue::ULong(x) => Value::Int(*x as i64),
                ColumnValue::Float(x) => Value::Float(f64::from(*x)),
                ColumnValue::Double(x) => Value::Float(*x),
                ColumnValue::String(s) | ColumnValue::Json(s) | ColumnValue::DateTime(s) => {
                    Value::Text((*s).to_string())
                }
                ColumnValue::Binary(_) => Value::Null,
            };
        }
        Ok(false)
    }
}

pub(super) fn read(path: &Path) -> Result<Layer> {
    let mut file = BufReader::new(File::open(path)?);
    let reader = FgbReader::open(&mut file).map_err(err)?;
    let header = reader.header();
    let fields: Vec<Field> = header
        .columns()
        .map(|cols| {
            cols.iter()
                .map(|c| {
                    let kind = match c.type_() {
                        ColumnType::Bool => FieldType::Bool,
                        ColumnType::Float | ColumnType::Double => FieldType::Float,
                        ColumnType::String
                        | ColumnType::Json
                        | ColumnType::DateTime
                        | ColumnType::Binary => FieldType::Text,
                        _ => FieldType::Int,
                    };
                    Field::new(c.name(), kind)
                })
                .collect()
        })
        .unwrap_or_default();
    // Our writer, like others, stores an empty CRS table for a layer without CRS.
    let crs = header
        .crs()
        .map(|c| Crs {
            epsg: (c.code() != 0).then_some(c.code()),
            wkt: c.wkt().map(str::to_string),
        })
        .filter(|c| c.epsg.is_some() || c.wkt.is_some());
    let name = header
        .name()
        .filter(|n| !n.is_empty())
        .map_or_else(|| stem(path), str::to_string);

    let mut features = Vec::new();
    let mut iter = reader.select_all().map_err(err)?;
    while let Some(feat) = iter.next().map_err(err)? {
        let geometry = feat.to_geo().ok();
        let mut row = Row(vec![Value::Null; fields.len()]);
        feat.process_properties(&mut row).map_err(err)?;
        features.push(Feature {
            geometry,
            values: row.0,
        });
    }
    Ok(Layer {
        name,
        fields,
        features,
        crs,
    })
}

/// Header geometry type: the common type, promoted to multi when singles and multis mix.
fn geometry_type(layer: &Layer) -> GeometryType {
    let mut ty: Option<GeometryType> = None;
    for g in layer.features.iter().filter_map(|f| f.geometry.as_ref()) {
        let t = match g {
            Geometry::Point(_) => GeometryType::Point,
            Geometry::MultiPoint(_) => GeometryType::MultiPoint,
            Geometry::Line(_) | Geometry::LineString(_) => GeometryType::LineString,
            Geometry::MultiLineString(_) => GeometryType::MultiLineString,
            Geometry::Polygon(_) | Geometry::Rect(_) | Geometry::Triangle(_) => {
                GeometryType::Polygon
            }
            Geometry::MultiPolygon(_) => GeometryType::MultiPolygon,
            Geometry::GeometryCollection(_) => return GeometryType::Unknown,
        };
        ty = Some(match ty {
            None => t,
            Some(a) if a == t => a,
            Some(a) => match (a, t) {
                (GeometryType::Point, GeometryType::MultiPoint)
                | (GeometryType::MultiPoint, GeometryType::Point) => GeometryType::MultiPoint,
                (GeometryType::LineString, GeometryType::MultiLineString)
                | (GeometryType::MultiLineString, GeometryType::LineString) => {
                    GeometryType::MultiLineString
                }
                (GeometryType::Polygon, GeometryType::MultiPolygon)
                | (GeometryType::MultiPolygon, GeometryType::Polygon) => GeometryType::MultiPolygon,
                _ => return GeometryType::Unknown,
            },
        });
    }
    ty.unwrap_or(GeometryType::Unknown)
}

pub(super) fn write(layer: &Layer, path: &Path) -> Result<()> {
    let wkt = layer.crs.as_ref().and_then(|c| c.wkt.as_deref());
    let options = FgbWriterOptions {
        crs: FgbCrs {
            code: layer.crs.as_ref().and_then(|c| c.epsg).unwrap_or(0),
            wkt,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut fgb =
        FgbWriter::create_with_options(&layer.name, geometry_type(layer), options).map_err(err)?;
    for field in &layer.fields {
        let ty = match field.kind {
            FieldType::Bool => ColumnType::Bool,
            FieldType::Int => ColumnType::Long,
            FieldType::Float => ColumnType::Double,
            FieldType::Text => ColumnType::String,
        };
        fgb.add_column(&field.name, ty, |_, col| col.nullable = true);
    }
    for (k, feat) in layer.features.iter().enumerate() {
        let geom = feat.geometry.clone().ok_or_else(|| Error::Format {
            format: FORMAT,
            message: format!("feature {k} has no geometry"),
        })?;
        let mut prop_err = None;
        fgb.add_feature_geom(geom, |fw| {
            for (i, (field, v)) in layer.fields.iter().zip(&feat.values).enumerate() {
                // Values follow the column type, whatever variant they were stored as.
                let text;
                let cv = match (field.kind, v) {
                    (_, Value::Null) => continue,
                    (FieldType::Bool, Value::Bool(b)) => ColumnValue::Bool(*b),
                    (FieldType::Int, Value::Int(x)) => ColumnValue::Long(*x),
                    (FieldType::Int, v) => match v.as_f64() {
                        Some(x) => ColumnValue::Long(x.round() as i64),
                        None => continue,
                    },
                    (FieldType::Float, v) => match v.as_f64() {
                        Some(x) => ColumnValue::Double(x),
                        None => continue,
                    },
                    (FieldType::Text, Value::Text(s)) => ColumnValue::String(s),
                    (FieldType::Text | FieldType::Bool, v) => {
                        text = v.to_string();
                        ColumnValue::String(&text)
                    }
                };
                if let Err(e) = fw.property(i, &field.name, &cv) {
                    prop_err.get_or_insert(e);
                }
            }
        })
        .map_err(err)?;
        if let Some(e) = prop_err {
            return Err(err(e));
        }
    }
    fgb.write(BufWriter::new(File::create(path)?)).map_err(err)
}
