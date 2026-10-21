//! In-memory vector layer: a schema, features with geometries and attribute values, and a CRS.
//!
//! The model is deliberately small so every format backend can map to and from it.

use geo::{BoundingRect, Validation};
use geo_types::{Geometry, Rect};
use rayon::prelude::*;

use crate::error::{Error, Result};

/// Attribute value attached to a feature.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl Value {
    /// Numeric view of the value. Text is parsed so that numbers stored as strings
    /// (common in CSV-derived shapefiles) still work; `None` for null or non-numeric text.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Null => None,
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Text(s) => s.trim().parse().ok(),
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Text(s) => f.write_str(s),
        }
    }
}

/// Type of an attribute column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Bool,
    Int,
    Float,
    Text,
}

/// Attribute column definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub kind: FieldType,
}

impl Field {
    pub fn new(name: impl Into<String>, kind: FieldType) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// A geometry and its attribute values, in the order of [`Layer::fields`].
#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    pub geometry: Option<Geometry<f64>>,
    pub values: Vec<Value>,
}

/// Coordinate reference system as found in the source file.
///
/// We only carry it through to the output; the cartogram itself works in whatever
/// planar units the input uses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Crs {
    pub epsg: Option<i32>,
    pub wkt: Option<String>,
}

impl Crs {
    /// Whether the CRS is geographic (lat/lon). Diffusion cartograms assume planar,
    /// ideally equal-area coordinates, so callers should warn when this is true.
    pub fn is_geographic(&self) -> bool {
        if matches!(self.epsg, Some(4326 | 4258 | 4269 | 4230 | 4171)) {
            return true;
        }
        self.wkt.as_deref().is_some_and(|wkt| {
            let head = wkt.trim_start().to_ascii_uppercase();
            head.starts_with("GEOGCS")
                || head.starts_with("GEOGCRS")
                || head.starts_with("GEOGRAPHICCRS")
        })
    }
}

/// A named collection of features sharing one schema.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Layer {
    pub name: String,
    pub fields: Vec<Field>,
    pub features: Vec<Feature>,
    pub crs: Option<Crs>,
}

impl Layer {
    pub fn new(name: impl Into<String>, fields: Vec<Field>) -> Self {
        Self {
            name: name.into(),
            fields,
            features: Vec::new(),
            crs: None,
        }
    }

    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    /// Returns the index of the field, appending it (with nulls) if it does not exist yet.
    pub fn ensure_field(&mut self, name: &str, kind: FieldType) -> usize {
        if let Some(idx) = self.field_index(name) {
            return idx;
        }
        self.fields.push(Field::new(name, kind));
        for feat in &mut self.features {
            feat.values.push(Value::Null);
        }
        self.fields.len() - 1
    }

    /// Numeric values of an attribute, one per feature (`None` for null or non-numeric).
    pub fn numeric_values(&self, name: &str) -> Result<Vec<Option<f64>>> {
        let idx = self
            .field_index(name)
            .ok_or_else(|| Error::AttributeNotFound(name.to_string()))?;
        Ok(self
            .features
            .iter()
            .map(|f| f.values.get(idx).and_then(Value::as_f64))
            .collect())
    }

    /// Indices of the features whose attribute holds no usable number: null, text that does
    /// not parse, NaN or infinite. A cartogram counts them as 0.
    pub fn features_without_value(&self, name: &str) -> Result<Vec<usize>> {
        Ok(self
            .numeric_values(name)?
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_some_and(f64::is_finite))
            .map(|(k, _)| k)
            .collect())
    }

    /// Whether every coordinate lies within longitude/latitude bounds (±180, ±90). For a
    /// layer without a CRS this is a hint, not proof: a small projected layer near the
    /// origin passes too, so use it to warn rather than to assume a CRS.
    pub fn looks_geographic(&self) -> bool {
        self.envelope().is_some_and(|e| {
            e.min().x >= -180.0 && e.max().x <= 180.0 && e.min().y >= -90.0 && e.max().y <= 90.0
        })
    }

    /// Bounding box of all geometries, `None` for an empty layer.
    pub fn envelope(&self) -> Option<Rect<f64>> {
        self.features
            .iter()
            .filter_map(|f| f.geometry.as_ref()?.bounding_rect())
            .reduce(|a, b| union_rect(&a, &b))
    }

    /// Indices of the features whose geometry is invalid under OGC rules (self-intersecting
    /// rings, overlapping parts…). Such polygons are rasterised by the even-odd rule, and
    /// their defects carry over to the cartogram.
    pub fn invalid_features(&self) -> Vec<usize> {
        self.features
            .par_iter()
            .enumerate()
            .filter(|(_, f)| f.geometry.as_ref().is_some_and(|g| !g.is_valid()))
            .map(|(k, _)| k)
            .collect()
    }
}

/// Smallest rectangle containing both inputs.
pub(crate) fn union_rect(a: &Rect<f64>, b: &Rect<f64>) -> Rect<f64> {
    Rect::new(
        (a.min().x.min(b.min().x), a.min().y.min(b.min().y)),
        (a.max().x.max(b.max().x), a.max().y.max(b.max().y)),
    )
}
