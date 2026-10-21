//! ESRI Shapefile (`.shp` + `.shx` + `.dbf`, optional `.prj`).

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use geo_types::{Geometry, MultiLineString, MultiPoint, MultiPolygon};
use shapefile::dbase::{self, FieldValue};
use shapefile::{Shape, ShapeReader};

use super::stem;
use crate::error::{Error, Result};
use crate::layer::{Crs, Feature, Field, FieldType, Layer, Value};

const FORMAT: &str = "shapefile";

pub(super) fn read(path: &Path) -> Result<Layer> {
    let shapes = ShapeReader::from_path(path)
        .and_then(|r| r.read())
        .map_err(|e| Error::format(FORMAT, e))?;

    let dbf_path = path.with_extension("dbf");
    let (fields, rows) = if dbf_path.exists() {
        read_dbf(&dbf_path)?
    } else {
        (Vec::new(), vec![Vec::new(); shapes.len()])
    };
    if rows.len() != shapes.len() {
        return Err(Error::Format {
            format: FORMAT,
            message: format!("{} shapes but {} dbf records", shapes.len(), rows.len()),
        });
    }

    let features = shapes
        .into_iter()
        .zip(rows)
        .map(|(shape, values)| {
            let geometry = match shape {
                Shape::NullShape => None,
                s => Some(single_part(
                    Geometry::try_from(s).map_err(|e| Error::format(FORMAT, e))?,
                )),
            };
            Ok(Feature { geometry, values })
        })
        .collect::<Result<Vec<_>>>()?;

    let crs = std::fs::read_to_string(path.with_extension("prj"))
        .ok()
        .map(|wkt| Crs {
            epsg: None,
            wkt: Some(wkt.trim().to_string()),
        });
    Ok(Layer {
        name: stem(path),
        fields,
        features,
        crs,
    })
}

/// Text encoding of a dbf the way GDAL resolves it: the `.cpg` sidecar first, then the
/// language driver byte for the ESRI marks the dbase crate does not know. `None` lets dbase
/// use the byte itself (falling back to lossy UTF-8).
fn dbf_encoding(dbf: &Path) -> Option<&'static dbase::encoding_rs::Encoding> {
    use dbase::encoding_rs::{Encoding, UTF_8, WINDOWS_1252};
    if let Ok(cpg) = std::fs::read_to_string(dbf.with_extension("cpg")) {
        let label = cpg.trim();
        let by_number = match label {
            "65001" => Some(UTF_8),
            "88591" | "8859_1" => Encoding::for_label(b"iso-8859-1"),
            n if n.len() == 4 && n.starts_with("125") => {
                Encoding::for_label(format!("windows-{n}").as_bytes())
            }
            _ => None,
        };
        if let Some(enc) = by_number.or_else(|| Encoding::for_label(label.as_bytes())) {
            return Some(enc);
        }
    }
    let mut header = [0u8; 32];
    std::fs::File::open(dbf)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut header))
        .ok()?;
    // 0x57 is ESRI's "ANSI" mark, i.e. the Windows code page, in practice 1252.
    (header[29] == 0x57).then_some(WINDOWS_1252)
}

fn read_dbf(path: &Path) -> Result<(Vec<Field>, Vec<Vec<Value>>)> {
    let options = dbase::ReadingOptions::default().character_trim(dbase::TrimOption::BeginEnd);
    let builder = dbase::ReaderBuilder::new().with_options(options);
    let builder = match dbf_encoding(path) {
        Some(enc) => builder.with_encoding(dbase::encoding::EncodingRs::from(enc)),
        None => builder,
    };
    let mut reader = builder.open(path).map_err(|e| Error::format(FORMAT, e))?;
    let infos: Vec<(String, dbase::FieldType)> = reader
        .fields()
        .iter()
        .filter(|f| f.name() != "DeletionFlag")
        .map(|f| (f.name().to_string(), f.field_type()))
        .collect();
    let records = reader.read().map_err(|e| Error::format(FORMAT, e))?;

    let rows: Vec<Vec<Value>> = records
        .iter()
        .map(|rec| {
            infos
                .iter()
                .map(|(name, _)| rec.get(name).map_or(Value::Null, to_value))
                .collect()
        })
        .collect();

    // dBase numeric columns are decimal strings; call them integers when every value is.
    let fields = infos
        .iter()
        .enumerate()
        .map(|(k, (name, ty))| {
            let kind = match ty {
                dbase::FieldType::Character | dbase::FieldType::Memo => FieldType::Text,
                dbase::FieldType::Date | dbase::FieldType::DateTime => FieldType::Text,
                dbase::FieldType::Logical => FieldType::Bool,
                dbase::FieldType::Integer => FieldType::Int,
                dbase::FieldType::Numeric => {
                    let integral = rows.iter().all(|r| match r[k] {
                        Value::Float(f) => f.fract() == 0.0 && f.abs() < 9.0e15,
                        _ => true,
                    });
                    if integral {
                        FieldType::Int
                    } else {
                        FieldType::Float
                    }
                }
                _ => FieldType::Float,
            };
            Field::new(name.clone(), kind)
        })
        .collect::<Vec<_>>();

    let rows = rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .zip(&fields)
                .map(|(v, f)| match (v, f.kind) {
                    (Value::Float(x), FieldType::Int) => Value::Int(x as i64),
                    (v, _) => v,
                })
                .collect()
        })
        .collect();
    Ok((fields, rows))
}

fn to_value(v: &FieldValue) -> Value {
    match v {
        FieldValue::Character(Some(s)) => Value::Text(s.clone()),
        FieldValue::Memo(s) => Value::Text(s.clone()),
        FieldValue::Numeric(Some(x)) => Value::Float(*x),
        FieldValue::Float(Some(x)) => Value::Float(f64::from(*x)),
        FieldValue::Double(x) | FieldValue::Currency(x) => Value::Float(*x),
        FieldValue::Integer(i) => Value::Int(i64::from(*i)),
        FieldValue::Logical(Some(b)) => Value::Bool(*b),
        FieldValue::Date(Some(d)) => {
            Value::Text(format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day()))
        }
        FieldValue::DateTime(dt) => {
            let (d, t) = (dt.date(), dt.time());
            Value::Text(format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                d.year(),
                d.month(),
                d.day(),
                t.hours(),
                t.minutes(),
                t.seconds()
            ))
        }
        _ => Value::Null,
    }
}

/// Shapefiles always store multi-part shapes; unwrap the common single-part case.
fn single_part(geom: Geometry<f64>) -> Geometry<f64> {
    match geom {
        Geometry::MultiPolygon(mut mp) if mp.0.len() == 1 => Geometry::Polygon(mp.0.remove(0)),
        Geometry::MultiLineString(mut ml) if ml.0.len() == 1 => {
            Geometry::LineString(ml.0.remove(0))
        }
        g => g,
    }
}

/// Shape family of a layer: a shapefile holds a single one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    Point,
    MultiPoint,
    Line,
    Polygon,
}

fn family(g: &Geometry<f64>) -> Option<Family> {
    match g {
        Geometry::Point(_) => Some(Family::Point),
        Geometry::MultiPoint(_) => Some(Family::MultiPoint),
        Geometry::Line(_) | Geometry::LineString(_) | Geometry::MultiLineString(_) => {
            Some(Family::Line)
        }
        Geometry::Polygon(_)
        | Geometry::MultiPolygon(_)
        | Geometry::Rect(_)
        | Geometry::Triangle(_) => Some(Family::Polygon),
        Geometry::GeometryCollection(_) => None,
    }
}

pub(super) fn write(layer: &Layer, path: &Path) -> Result<()> {
    let fam = layer
        .features
        .iter()
        .filter_map(|f| f.geometry.as_ref())
        .map(|g| family(g).ok_or_else(|| mixed("geometry collections")))
        .try_fold(None, |acc: Option<Family>, f| {
            let f = f?;
            match acc {
                Some(a) if a != f => Err(mixed(&format!("{a:?} and {f:?} geometries"))),
                _ => Ok(Some(f)),
            }
        })?
        .unwrap_or(Family::Polygon);
    // Single points and multipoints can share a Multipoint file.
    let fam = if fam == Family::Point
        && layer
            .features
            .iter()
            .any(|f| matches!(f.geometry, Some(Geometry::MultiPoint(_))))
    {
        Family::MultiPoint
    } else {
        fam
    };

    // Checked before any file is created. The shapefile crate writes every record with the
    // header's shape type, so it has no way to store a null shape.
    if let Some(k) = layer.features.iter().position(|f| f.geometry.is_none()) {
        return Err(Error::Format {
            format: FORMAT,
            message: format!(
                "feature {k} has no geometry, which this writer cannot store in a shapefile; use GeoPackage, GeoJSON, FlatGeobuf or TopoJSON"
            ),
        });
    }

    let names = dbf_names(layer);
    let mut widths = Vec::with_capacity(layer.fields.len());
    let mut builder = dbase::TableWriterBuilder::new();
    for (k, (field, name)) in layer.fields.iter().zip(&names).enumerate() {
        let fname =
            dbase::FieldName::try_from(name.as_str()).map_err(|e| Error::format(FORMAT, e))?;
        // Text columns are as wide as their longest value, up to dBase's 254 bytes.
        let width = layer
            .features
            .iter()
            .map(|f| match &f.values[k] {
                Value::Text(s) => s.len(),
                v => v.to_string().len(),
            })
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_TEXT_BYTES);
        widths.push(width);
        builder = match field.kind {
            FieldType::Bool => builder.add_logical_field(fname),
            FieldType::Int => builder.add_numeric_field(fname, 18, 0),
            FieldType::Float => builder.add_numeric_field(fname, 24, 10),
            FieldType::Text => builder.add_character_field(fname, width as u8),
        };
    }

    let written = shapefile::Writer::from_path(path, builder)
        .map_err(|e| Error::format(FORMAT, e))
        .and_then(|mut writer| write_records(&mut writer, layer, fam, &names, &widths));
    if written.is_err() {
        // Don't leave a .shp whose records disagree with its .dbf.
        for ext in ["shp", "shx", "dbf"] {
            let _ = std::fs::remove_file(path.with_extension(ext));
        }
    }
    written?;

    if let Some(wkt) = layer.crs.as_ref().and_then(|c| c.wkt.as_deref()) {
        std::fs::write(path.with_extension("prj"), wkt)?;
    }
    Ok(())
}

/// Longest text a dBase character column holds, in bytes.
const MAX_TEXT_BYTES: usize = 254;

/// Writes every feature; `widths` are the column widths, in bytes, for text values.
fn write_records(
    writer: &mut shapefile::Writer<BufWriter<File>>,
    layer: &Layer,
    fam: Family,
    names: &[String],
    widths: &[usize],
) -> Result<()> {
    for feat in &layer.features {
        let mut record = dbase::Record::default();
        for (((field, name), value), &width) in
            layer.fields.iter().zip(names).zip(&feat.values).zip(widths)
        {
            record.insert(name.clone(), to_field_value(value, field.kind, width));
        }
        let geom = feat
            .geometry
            .as_ref()
            .expect("null geometries rejected by the caller");
        let res = match fam {
            Family::Point => match geom {
                Geometry::Point(p) => {
                    writer.write_shape_and_record(&shapefile::Point::from(*p), &record)
                }
                _ => unreachable!("family checked above"),
            },
            Family::MultiPoint => {
                let mp = match geom {
                    Geometry::Point(p) => MultiPoint(vec![*p]),
                    Geometry::MultiPoint(mp) => mp.clone(),
                    _ => unreachable!("family checked above"),
                };
                writer.write_shape_and_record(&shapefile::Multipoint::from(mp), &record)
            }
            Family::Line => {
                let ml = match geom {
                    Geometry::Line(l) => MultiLineString::from(geo_types::LineString::from(*l)),
                    Geometry::LineString(ls) => MultiLineString(vec![ls.clone()]),
                    Geometry::MultiLineString(ml) => ml.clone(),
                    _ => unreachable!("family checked above"),
                };
                writer.write_shape_and_record(&shapefile::Polyline::from(ml), &record)
            }
            Family::Polygon => {
                let mp = match geom {
                    Geometry::Polygon(p) => MultiPolygon(vec![p.clone()]),
                    Geometry::MultiPolygon(mp) => mp.clone(),
                    Geometry::Rect(r) => MultiPolygon(vec![r.to_polygon()]),
                    Geometry::Triangle(t) => MultiPolygon(vec![t.to_polygon()]),
                    _ => unreachable!("family checked above"),
                };
                writer.write_shape_and_record(&shapefile::Polygon::from(mp), &record)
            }
        };
        res.map_err(|e| Error::format(FORMAT, e))?;
    }
    Ok(())
}

fn mixed(what: &str) -> Error {
    Error::Format {
        format: FORMAT,
        message: format!("a shapefile holds one geometry type, this layer has {what}; use GeoPackage, GeoJSON or FlatGeobuf"),
    }
}

/// dBase field names are limited to 10 characters; truncate and make them unique.
fn dbf_names(layer: &Layer) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for field in &layer.fields {
        let base: String = field.name.chars().take(10).collect();
        let mut name = base.clone();
        let mut n = 1;
        while names.contains(&name) {
            let suffix = n.to_string();
            name = format!(
                "{}{}",
                base.chars().take(10 - suffix.len()).collect::<String>(),
                suffix
            );
            n += 1;
        }
        names.push(name);
    }
    names
}

/// dBase value for a column of type `kind`; text is cut to `width` bytes, on a character
/// boundary, as GDAL does for values longer than a dBase column can hold.
fn to_field_value(v: &Value, kind: FieldType, width: usize) -> FieldValue {
    match kind {
        FieldType::Bool => FieldValue::Logical(match v {
            Value::Bool(b) => Some(*b),
            _ => None,
        }),
        FieldType::Int | FieldType::Float => FieldValue::Numeric(v.as_f64()),
        FieldType::Text => FieldValue::Character(match v {
            Value::Null => None,
            v => {
                let mut s = v.to_string();
                let mut end = s.len().min(width);
                while !s.is_char_boundary(end) {
                    end -= 1;
                }
                s.truncate(end);
                Some(s)
            }
        }),
    }
}
