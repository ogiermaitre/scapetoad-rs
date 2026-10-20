//! TopoJSON: several layers (objects) in one file, sharing their borders as arcs.
//!
//! The topology itself is built by [`crate::topology`]; this module maps it to and from JSON.
//! Like GeoJSON, the file carries the coordinates we have, projected ones included, plus a
//! non-standard `crs` member when the layers have an EPSG code. A file is always written
//! whole, since arcs are shared across all its objects.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use serde_json::{Map, Value as JsonValue};

use super::infer_schema;
use super::json::{crs_member, json_to_value, legacy_crs, value_to_json};
use crate::error::{Error, Result};
use crate::layer::{Crs, Feature, Layer};
use crate::topology::{ArcRef, Position, TopoGeometry, Topology, Transform};

const FORMAT: &str = "topojson";

fn error(message: impl std::fmt::Display) -> Error {
    Error::format(FORMAT, message)
}

/// Writes layers as the objects of one topology, named after [`Layer::name`].
pub(super) fn write(layers: &[&Layer], path: &Path, quantization: Option<u32>) -> Result<()> {
    for (i, layer) in layers.iter().enumerate() {
        if layers[..i].iter().any(|l| l.name == layer.name) {
            return Err(error(format!("two layers named '{}'", layer.name)));
        }
    }
    let crs = common_crs(layers)?;
    let objects: Vec<(&str, Vec<_>)> = layers
        .iter()
        .map(|l| {
            let geoms = l.features.iter().map(|f| f.geometry.as_ref()).collect();
            (l.name.as_str(), geoms)
        })
        .collect();
    let topo = Topology::build(&objects, quantization);
    let quantized = topo.transform.is_some();

    let mut root = Map::new();
    root.insert("type".into(), "Topology".into());
    if let Some(bbox) = topo.bbox {
        root.insert("bbox".into(), bbox.iter().map(|&v| number(v)).collect());
    }
    if let Some(t) = &topo.transform {
        root.insert(
            "transform".into(),
            serde_json::json!({ "scale": t.scale, "translate": t.translate }),
        );
    }
    if let Some(member) = crs_member(crs.as_ref()) {
        root.insert("crs".into(), member);
    }
    let mut objects = Map::new();
    for (layer, (name, geoms)) in layers.iter().zip(&topo.objects) {
        let geometries: Vec<JsonValue> = geoms
            .iter()
            .zip(&layer.features)
            .map(|(g, f)| {
                let mut obj = geometry_json(g, quantized);
                let properties: Map<String, JsonValue> = layer
                    .fields
                    .iter()
                    .zip(&f.values)
                    .map(|(field, v)| (field.name.clone(), value_to_json(v)))
                    .collect();
                obj.insert("properties".into(), properties.into());
                obj.into()
            })
            .collect();
        objects.insert(
            name.clone(),
            serde_json::json!({ "type": "GeometryCollection", "geometries": geometries }),
        );
    }
    root.insert("objects".into(), objects.into());
    root.insert(
        "arcs".into(),
        topo.arcs.iter().map(|a| arc_json(a, quantized)).collect(),
    );

    let mut out = BufWriter::new(File::create(path)?);
    serde_json::to_writer(&mut out, &root).map_err(error)?;
    out.flush()?;
    Ok(())
}

/// The CRS shared by all layers; layers without one are assumed to match the others.
fn common_crs(layers: &[&Layer]) -> Result<Option<Crs>> {
    let mut found: Option<&Crs> = None;
    for crs in layers.iter().filter_map(|l| l.crs.as_ref()) {
        match found {
            Some(c) if c.epsg.is_some() && crs.epsg.is_some() && c.epsg != crs.epsg => {
                return Err(error("layers in one file must share a coordinate system"));
            }
            Some(_) => {}
            None => found = Some(crs),
        }
    }
    Ok(found.cloned())
}

/// A finite number, or `null` (JSON has no NaN or infinity).
fn number(v: f64) -> JsonValue {
    serde_json::Number::from_f64(v).map_or(JsonValue::Null, JsonValue::Number)
}

/// A position: integers when quantized, so the file holds `123` rather than `123.0`.
fn position_json(p: Position, quantized: bool) -> JsonValue {
    if quantized {
        JsonValue::from(vec![p[0] as i64, p[1] as i64])
    } else {
        JsonValue::from(vec![number(p[0]), number(p[1])])
    }
}

/// Quantized arcs are delta-encoded: the first position, then differences.
fn arc_json(arc: &[Position], quantized: bool) -> JsonValue {
    if !quantized {
        return arc.iter().map(|&p| position_json(p, false)).collect();
    }
    let mut prev = [0.0, 0.0];
    arc.iter()
        .map(|p| {
            let delta = [p[0] - prev[0], p[1] - prev[1]];
            prev = *p;
            position_json(delta, true)
        })
        .collect()
}

fn geometry_json(g: &TopoGeometry, quantized: bool) -> Map<String, JsonValue> {
    let refs = |r: &Vec<ArcRef>| JsonValue::from(r.clone());
    let rings = |rs: &Vec<Vec<ArcRef>>| rs.iter().map(refs).collect::<JsonValue>();
    let (kind, key, value): (JsonValue, &str, JsonValue) = match g {
        TopoGeometry::Null => (JsonValue::Null, "", JsonValue::Null),
        TopoGeometry::Point(p) => ("Point".into(), "coordinates", position_json(*p, quantized)),
        TopoGeometry::MultiPoint(ps) => (
            "MultiPoint".into(),
            "coordinates",
            ps.iter().map(|&p| position_json(p, quantized)).collect(),
        ),
        TopoGeometry::LineString(r) => ("LineString".into(), "arcs", refs(r)),
        TopoGeometry::MultiLineString(ls) => ("MultiLineString".into(), "arcs", rings(ls)),
        TopoGeometry::Polygon(rs) => ("Polygon".into(), "arcs", rings(rs)),
        TopoGeometry::MultiPolygon(ps) => (
            "MultiPolygon".into(),
            "arcs",
            ps.iter().map(rings).collect(),
        ),
        TopoGeometry::GeometryCollection(gs) => (
            "GeometryCollection".into(),
            "geometries",
            gs.iter()
                .map(|g| JsonValue::from(geometry_json(g, quantized)))
                .collect(),
        ),
    };
    let mut obj = Map::new();
    obj.insert("type".into(), kind);
    if !key.is_empty() {
        obj.insert(key.into(), value);
    }
    obj
}

/// Reads one object of a topology as a layer. Without a name, the file must hold a single
/// object.
pub(super) fn read(path: &Path, object: Option<&str>) -> Result<Layer> {
    let root: JsonValue =
        serde_json::from_reader(BufReader::new(File::open(path)?)).map_err(error)?;
    if root.get("type").and_then(JsonValue::as_str) != Some("Topology") {
        return Err(error("not a TopoJSON Topology"));
    }
    let transform = root.get("transform").map(parse_transform).transpose()?;
    let arcs = root
        .get("arcs")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| error("missing arcs"))?
        .iter()
        .map(|a| parse_arc(a, transform.is_some()))
        .collect::<Result<Vec<_>>>()?;
    let objects = root
        .get("objects")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| error("missing objects"))?;
    let (name, object) = match object {
        Some(name) => objects
            .get_key_value(name)
            .ok_or_else(|| error(format!("no object '{name}' in {}", path.display())))?,
        None if objects.len() == 1 => objects.iter().next().expect("one object"),
        None => {
            let names: Vec<&str> = objects.keys().map(String::as_str).collect();
            return Err(error(format!(
                "{} holds several objects ({}); choose one with {}#name",
                path.display(),
                names.join(", "),
                path.display()
            )));
        }
    };

    // A GeometryCollection object is a layer of features; any other geometry is one feature.
    let members: Vec<&JsonValue> = match object.get("type").and_then(JsonValue::as_str) {
        Some("GeometryCollection") => object
            .get("geometries")
            .and_then(JsonValue::as_array)
            .map(|gs| gs.iter().collect())
            .unwrap_or_default(),
        _ => vec![object],
    };
    let topo = Topology {
        bbox: None,
        transform,
        objects: Vec::new(),
        arcs,
    };
    let mut geoms = Vec::with_capacity(members.len());
    let mut rows = Vec::with_capacity(members.len());
    for m in members {
        geoms.push(topo.geometry(&parse_geometry(m)?));
        rows.push(
            m.get("properties")
                .and_then(JsonValue::as_object)
                .map(|p| {
                    p.iter()
                        .map(|(k, v)| (k.clone(), json_to_value(v.clone())))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        );
    }
    let (fields, values) = infer_schema(&rows);
    Ok(Layer {
        name: name.clone(),
        fields,
        features: geoms
            .into_iter()
            .zip(values)
            .map(|(geometry, values)| Feature { geometry, values })
            .collect(),
        crs: legacy_crs(&root),
    })
}

fn pair(v: &JsonValue) -> Result<[f64; 2]> {
    let a = v.as_array().filter(|a| a.len() >= 2);
    let get = |i: usize| a.and_then(|a| a[i].as_f64());
    match (get(0), get(1)) {
        (Some(x), Some(y)) => Ok([x, y]),
        _ => Err(error(format!("expected a position, got {v}"))),
    }
}

fn parse_transform(v: &JsonValue) -> Result<Transform> {
    let scale = v
        .get("scale")
        .ok_or_else(|| error("transform without scale"))?;
    let translate = v
        .get("translate")
        .ok_or_else(|| error("transform without translate"))?;
    Ok(Transform {
        scale: pair(scale)?,
        translate: pair(translate)?,
    })
}

/// Absolute positions of an arc, undoing the delta encoding of quantized files.
fn parse_arc(v: &JsonValue, delta: bool) -> Result<Vec<Position>> {
    let points = v
        .as_array()
        .ok_or_else(|| error("an arc is not an array"))?;
    let mut prev = [0.0, 0.0];
    points
        .iter()
        .map(|p| {
            let p = pair(p)?;
            if delta {
                prev = [prev[0] + p[0], prev[1] + p[1]];
                Ok(prev)
            } else {
                Ok(p)
            }
        })
        .collect()
}

fn parse_refs(v: &JsonValue) -> Result<Vec<ArcRef>> {
    v.as_array()
        .ok_or_else(|| error("expected a list of arcs"))?
        .iter()
        .map(|r| {
            r.as_i64()
                .ok_or_else(|| error("an arc index is not an integer"))
        })
        .collect()
}

fn parse_list<T>(v: Option<&JsonValue>, item: impl Fn(&JsonValue) -> Result<T>) -> Result<Vec<T>> {
    v.and_then(JsonValue::as_array)
        .ok_or_else(|| error("expected an array"))?
        .iter()
        .map(item)
        .collect()
}

fn parse_geometry(v: &JsonValue) -> Result<TopoGeometry> {
    let arcs = v.get("arcs");
    let coords = v.get("coordinates");
    Ok(match v.get("type").and_then(JsonValue::as_str) {
        None => TopoGeometry::Null,
        Some("Point") => TopoGeometry::Point(pair(coords.unwrap_or(&JsonValue::Null))?),
        Some("MultiPoint") => TopoGeometry::MultiPoint(parse_list(coords, pair)?),
        Some("LineString") => {
            TopoGeometry::LineString(parse_refs(arcs.unwrap_or(&JsonValue::Null))?)
        }
        Some("MultiLineString") => TopoGeometry::MultiLineString(parse_list(arcs, parse_refs)?),
        Some("Polygon") => TopoGeometry::Polygon(parse_list(arcs, parse_refs)?),
        Some("MultiPolygon") => {
            TopoGeometry::MultiPolygon(parse_list(arcs, |p| parse_list(Some(p), parse_refs))?)
        }
        Some("GeometryCollection") => {
            TopoGeometry::GeometryCollection(parse_list(v.get("geometries"), parse_geometry)?)
        }
        Some(other) => return Err(error(format!("unknown geometry type '{other}'"))),
    })
}
