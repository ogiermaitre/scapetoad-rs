//! `Layer` and `Crs`, with the conversions of geometries (WKB) and attribute values.

use std::borrow::Cow;
use std::path::PathBuf;

use geozero::wkb::Wkb;
use geozero::{CoordDimensions, ToGeo, ToWkb};
use pyo3::exceptions::{PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use pyo3::IntoPyObjectExt;
use scapetoad::geo_types::Geometry;
use scapetoad::{Crs, Feature, Field, FieldType, Layer, Value};

use crate::{py_err, ScapeToadError};

/// Coordinate reference system, carried through to the outputs (never used to reproject).
#[pyclass(name = "Crs", module = "scapetoad", frozen, eq)]
#[derive(PartialEq)]
pub struct PyCrs {
    pub inner: Crs,
}

#[pymethods]
impl PyCrs {
    #[new]
    #[pyo3(signature = (epsg = None, wkt = None))]
    fn new(epsg: Option<i32>, wkt: Option<String>) -> Self {
        Self {
            inner: Crs { epsg, wkt },
        }
    }

    #[getter]
    fn epsg(&self) -> Option<i32> {
        self.inner.epsg
    }

    #[getter]
    fn wkt(&self) -> Option<&str> {
        self.inner.wkt.as_deref()
    }

    /// Whether the CRS is longitude/latitude, which cartograms should not be computed in.
    #[getter]
    fn is_geographic(&self) -> bool {
        self.inner.is_geographic()
    }

    fn __repr__(&self) -> String {
        match (&self.inner.epsg, &self.inner.wkt) {
            (Some(epsg), _) => format!("Crs(epsg={epsg})"),
            (None, Some(wkt)) => {
                let head: String = wkt.chars().take(40).collect();
                let more = if head.len() < wkt.len() { "…" } else { "" };
                format!("Crs(wkt={head:?}{more})")
            }
            (None, None) => "Crs()".to_string(),
        }
    }
}

/// A vector layer: geometries, attribute columns and a CRS, held on the Rust side.
#[pyclass(name = "Layer", module = "scapetoad")]
pub struct PyLayer {
    pub inner: Layer,
}

impl From<Layer> for PyLayer {
    fn from(inner: Layer) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl PyLayer {
    /// Builds a layer from WKB geometries (`None` for a missing one) and attribute columns.
    ///
    /// Column types are inferred from the values: `bool`, `int`, `float` or `str`, widening
    /// int to float and anything mixed with text to text. `None` is a null value.
    #[staticmethod]
    #[pyo3(signature = (geometries, columns = None, *, name = "layer", crs = None))]
    fn from_wkb(
        geometries: &Bound<'_, PyAny>,
        columns: Option<&Bound<'_, PyAny>>,
        name: &str,
        crs: Option<PyRef<'_, PyCrs>>,
    ) -> PyResult<Self> {
        let geometries = geometries_from_wkb(geometries)?;
        let mut fields = Vec::new();
        let mut values: Vec<Vec<Value>> = vec![Vec::new(); geometries.len()];
        if let Some(columns) = columns {
            for item in columns.call_method0("items")?.try_iter()? {
                let (key, column): (String, Bound<'_, PyAny>) = item?.extract()?;
                let column = column
                    .try_iter()?
                    .map(|v| value_from_py(&v?))
                    .collect::<PyResult<Vec<Value>>>()?;
                if column.len() != geometries.len() {
                    return Err(PyValueError::new_err(format!(
                        "column '{key}' has {} values for {} geometries",
                        column.len(),
                        geometries.len()
                    )));
                }
                let kind = column_type(&column);
                for (row, v) in values.iter_mut().zip(column) {
                    row.push(coerce(v, kind));
                }
                fields.push(Field::new(key, kind));
            }
        }

        let mut layer = Layer::new(name, fields);
        layer.crs = crs.map(|c| c.inner.clone());
        layer.features = geometries
            .into_iter()
            .zip(values)
            .map(|(geometry, values)| Feature { geometry, values })
            .collect();
        Ok(Self { inner: layer })
    }

    /// Writes the layer; see `scapetoad.write_layer`.
    fn write(&self, py: Python<'_>, target: PathBuf) -> PyResult<()> {
        let layer = &self.inner;
        py.detach(|| scapetoad::io::write_layer(layer, &target))
            .map_err(py_err)
    }

    #[getter]
    fn name(&self) -> &str {
        &self.inner.name
    }

    #[setter]
    fn set_name(&mut self, name: String) {
        self.inner.name = name;
    }

    /// Attribute columns as `(name, type)` pairs, type one of `bool`, `int`, `float`, `str`.
    #[getter]
    fn fields(&self) -> Vec<(String, &'static str)> {
        self.inner
            .fields
            .iter()
            .map(|f| (f.name.clone(), type_name(f.kind)))
            .collect()
    }

    #[getter]
    fn crs(&self) -> Option<PyCrs> {
        self.inner.crs.clone().map(|inner| PyCrs { inner })
    }

    /// Sets the CRS, e.g. for a shapefile without `.prj`. Does not reproject.
    #[setter]
    fn set_crs(&mut self, crs: Option<PyRef<'_, PyCrs>>) {
        self.inner.crs = crs.map(|c| c.inner.clone());
    }

    /// Bounding box `(min_x, min_y, max_x, max_y)`, `None` for an empty layer.
    #[getter]
    fn envelope(&self) -> Option<(f64, f64, f64, f64)> {
        self.inner
            .envelope()
            .map(|r| (r.min().x, r.min().y, r.max().x, r.max().y))
    }

    /// Whether all coordinates fall within ±180/±90: a hint of lon/lat data, not a proof.
    fn looks_geographic(&self) -> bool {
        self.inner.looks_geographic()
    }

    /// Indices of the features whose geometry is invalid under OGC rules; such polygons are
    /// rasterised by the even-odd rule and their defects carry over to the cartogram.
    fn invalid_features(&self, py: Python<'_>) -> Vec<usize> {
        let layer = &self.inner;
        py.detach(|| layer.invalid_features())
    }

    /// Values of one attribute, one per feature.
    fn column<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Vec<Bound<'py, PyAny>>> {
        let idx = self
            .inner
            .field_index(name)
            .ok_or_else(|| PyKeyError::new_err(name.to_string()))?;
        self.inner
            .features
            .iter()
            .map(|f| value_to_py(py, f.values.get(idx).unwrap_or(&Value::Null)))
            .collect()
    }

    /// All attributes as `{name: [values…]}`, in column order.
    fn columns<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        for field in &self.inner.fields {
            dict.set_item(&field.name, self.column(py, &field.name)?)?;
        }
        Ok(dict)
    }

    /// Geometries as 2D WKB (`None` for a feature without geometry).
    fn to_wkb<'py>(&self, py: Python<'py>) -> PyResult<Vec<Option<Bound<'py, PyBytes>>>> {
        geometries_to_wkb(py, self.inner.features.iter().map(|f| f.geometry.as_ref()))
    }

    fn __len__(&self) -> usize {
        self.inner.features.len()
    }

    fn __repr__(&self) -> String {
        format!(
            "Layer({:?}, {} features, {} fields)",
            self.inner.name,
            self.inner.features.len(),
            self.inner.fields.len()
        )
    }
}

/// Decodes an iterable of WKB `bytes` or `None`.
pub(crate) fn geometries_from_wkb(
    geometries: &Bound<'_, PyAny>,
) -> PyResult<Vec<Option<Geometry<f64>>>> {
    geometries
        .try_iter()?
        .enumerate()
        .map(|(k, item)| {
            let item = item?;
            if item.is_none() {
                return Ok(None);
            }
            let bytes: Cow<'_, [u8]> = item.extract()?;
            Wkb(&bytes[..])
                .to_geo()
                .map(Some)
                .map_err(|e| PyValueError::new_err(format!("invalid WKB at index {k}: {e}")))
        })
        .collect()
}

/// Encodes geometries as 2D WKB, `None` staying `None`.
pub(crate) fn geometries_to_wkb<'a, 'py>(
    py: Python<'py>,
    geometries: impl IntoIterator<Item = Option<&'a Geometry<f64>>>,
) -> PyResult<Vec<Option<Bound<'py, PyBytes>>>> {
    geometries
        .into_iter()
        .map(|g| {
            g.map(|g| {
                g.to_wkb(CoordDimensions::xy())
                    .map(|b| PyBytes::new(py, &b))
                    .map_err(|e| ScapeToadError::new_err(format!("cannot encode WKB: {e}")))
            })
            .transpose()
        })
        .collect()
}

fn type_name(kind: FieldType) -> &'static str {
    match kind {
        FieldType::Bool => "bool",
        FieldType::Int => "int",
        FieldType::Float => "float",
        FieldType::Text => "str",
    }
}

fn value_to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    match value {
        Value::Null => Ok(py.None().into_bound(py)),
        Value::Bool(b) => b.into_bound_py_any(py),
        Value::Int(i) => i.into_bound_py_any(py),
        Value::Float(x) => x.into_bound_py_any(py),
        Value::Text(s) => s.into_bound_py_any(py),
    }
}

/// Python scalar to a value. `bool` comes first because Python's `bool` is an `int`
/// (PyO3 also accepts `numpy.bool_` here); other numpy scalars convert through `__index__`
/// and `__float__`.
fn value_from_py(obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.extract::<bool>() {
        return Ok(Value::Bool(b));
    }
    if let Ok(i) = obj.extract::<i64>() {
        return Ok(Value::Int(i));
    }
    if let Ok(x) = obj.extract::<f64>() {
        return Ok(Value::Float(x));
    }
    if let Ok(s) = obj.extract::<String>() {
        return Ok(Value::Text(s));
    }
    Err(PyTypeError::new_err(format!(
        "unsupported attribute value of type {}: use bool, int, float, str or None",
        obj.get_type().name()?
    )))
}

/// Narrowest type holding every value of a column; text for a column of nulls only.
fn column_type(values: &[Value]) -> FieldType {
    let rank = |v: &Value| match v {
        Value::Null => None,
        Value::Bool(_) => Some(FieldType::Bool),
        Value::Int(_) => Some(FieldType::Int),
        Value::Float(_) => Some(FieldType::Float),
        Value::Text(_) => Some(FieldType::Text),
    };
    values
        .iter()
        .filter_map(rank)
        .reduce(|a, b| match (a, b) {
            (a, b) if a == b => a,
            (FieldType::Text, _) | (_, FieldType::Text) => FieldType::Text,
            (FieldType::Float, _) | (_, FieldType::Float) => FieldType::Float,
            _ => FieldType::Int,
        })
        .unwrap_or(FieldType::Text)
}

/// Converts a value to the column type chosen by [`column_type`], which only widens.
fn coerce(value: Value, kind: FieldType) -> Value {
    match (value, kind) {
        (Value::Null, _) => Value::Null,
        (v @ Value::Text(_), FieldType::Text) => v,
        (v, FieldType::Text) => Value::Text(v.to_string()),
        (Value::Bool(b), FieldType::Int) => Value::Int(i64::from(b)),
        (v, FieldType::Float) => v.as_f64().map_or(Value::Null, Value::Float),
        (v, _) => v,
    }
}
