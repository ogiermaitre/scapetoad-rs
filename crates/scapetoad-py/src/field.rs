//! `DeformationField`: the reusable result of a computation.

use std::borrow::Cow;
use std::path::PathBuf;

use pyo3::exceptions::{PyIndexError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyTuple};
use scapetoad::geo_types::{Coord, Geometry, Rect};
use scapetoad::DeformationField;

use crate::layer::{geometries_from_wkb, geometries_to_wkb, PyCrs, PyLayer};
use crate::py_err;

/// Deformed grid over an envelope; projects any geometry inside it like the cartogram.
#[pyclass(name = "DeformationField", module = "scapetoad", frozen)]
pub struct PyField {
    pub inner: DeformationField,
}

#[pymethods]
impl PyField {
    /// Undeformed field of `nx` × `ny` nodes over `(min_x, min_y, max_x, max_y)`.
    #[staticmethod]
    fn identity(envelope: (f64, f64, f64, f64), nx: usize, ny: usize) -> PyResult<Self> {
        let (x0, y0, x1, y1) = envelope;
        let rect = Rect::new((x0, y0), (x1, y1));
        let inner = DeformationField::identity(rect, nx, ny).map_err(py_err)?;
        Ok(Self { inner })
    }

    /// Reads a `.stdf` file.
    #[staticmethod]
    fn load(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        let inner = py
            .detach(|| DeformationField::load(&path))
            .map_err(py_err)?;
        Ok(Self { inner })
    }

    /// Writes a `.stdf` file.
    fn save(&self, py: Python<'_>, path: PathBuf) -> PyResult<()> {
        py.detach(|| self.inner.save(&path)).map_err(py_err)
    }

    /// Parses the `.stdf` encoding.
    #[staticmethod]
    fn from_bytes(data: Cow<'_, [u8]>) -> PyResult<Self> {
        let inner = DeformationField::read_from(&data[..]).map_err(py_err)?;
        Ok(Self { inner })
    }

    /// The `.stdf` encoding, also used for pickling.
    fn to_bytes<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let mut buf = Vec::new();
        self.inner.write_to(&mut buf).map_err(py_err)?;
        Ok(PyBytes::new(py, &buf))
    }

    fn __reduce__<'py>(slf: &Bound<'py, Self>) -> PyResult<Bound<'py, PyTuple>> {
        let ctor = slf.get_type().getattr("from_bytes")?;
        let bytes = slf.get().to_bytes(slf.py())?;
        PyTuple::new(
            slf.py(),
            [ctor, PyTuple::new(slf.py(), [bytes])?.into_any()],
        )
    }

    /// `(min_x, min_y, max_x, max_y)` of the undeformed grid.
    #[getter]
    fn envelope(&self) -> (f64, f64, f64, f64) {
        let r = self.inner.envelope();
        (r.min().x, r.min().y, r.max().x, r.max().y)
    }

    /// Number of nodes `(nx, ny)`.
    #[getter]
    fn size(&self) -> (usize, usize) {
        self.inner.size()
    }

    /// Size `(width, height)` of an undeformed cell.
    #[getter]
    fn cell_size(&self) -> (f64, f64) {
        self.inner.cell_size()
    }

    #[getter]
    fn crs(&self) -> Option<PyCrs> {
        self.inner.crs.clone().map(|inner| PyCrs { inner })
    }

    /// Deformed position of node `(i, j)`, counted from the bottom-left corner.
    fn node(&self, i: usize, j: usize) -> PyResult<(f64, f64)> {
        let (nx, ny) = self.inner.size();
        if i >= nx || j >= ny {
            return Err(PyIndexError::new_err(format!(
                "node ({i}, {j}) outside a {nx}x{ny} grid"
            )));
        }
        let c = self.inner.node(i, j);
        Ok((c.x, c.y))
    }

    /// Projects one point.
    fn project(&self, x: f64, y: f64) -> (f64, f64) {
        let c = self.inner.project_coord(Coord { x, y });
        (c.x, c.y)
    }

    /// Projects a sequence of `(x, y)` points (an `(n, 2)` numpy array works too).
    fn project_points(&self, py: Python<'_>, points: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
        py.detach(|| {
            points
                .into_iter()
                .map(|(x, y)| {
                    let c = self.inner.project_coord(Coord { x, y });
                    (c.x, c.y)
                })
                .collect()
        })
    }

    /// Projects WKB geometries (`None` stays `None`); segments are split at grid lines so
    /// they follow the deformation.
    fn transform_wkb<'py>(
        &self,
        py: Python<'py>,
        geometries: &Bound<'py, PyAny>,
    ) -> PyResult<Vec<Option<Bound<'py, PyBytes>>>> {
        let geometries = geometries_from_wkb(geometries)?;
        let transformed: Vec<Option<Geometry<f64>>> = py.detach(|| {
            geometries
                .iter()
                .map(|g| g.as_ref().map(|g| self.inner.transform(g)))
                .collect()
        });
        geometries_to_wkb(py, transformed.iter().map(Option::as_ref))
    }

    /// Transforms every feature of a layer, keeping its attributes.
    fn transform_layer(&self, py: Python<'_>, layer: PyRef<'_, PyLayer>) -> PyLayer {
        let layer = &layer.inner;
        PyLayer::from(py.detach(|| self.inner.transform_layer(layer)))
    }

    /// Deformed graticule with about `lines` lines along the longer side.
    #[pyo3(signature = (lines = 100))]
    fn grid(&self, lines: usize) -> PyResult<PyLayer> {
        if lines == 0 {
            return Err(PyValueError::new_err("lines must be positive"));
        }
        Ok(PyLayer::from(self.inner.grid_layer(lines)))
    }

    fn __repr__(&self) -> String {
        let (nx, ny) = self.inner.size();
        let (x0, y0, x1, y1) = self.envelope();
        format!("DeformationField({nx}x{ny} nodes, envelope=({x0}, {y0}, {x1}, {y1}))")
    }
}
