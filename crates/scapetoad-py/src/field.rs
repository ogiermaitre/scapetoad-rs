//! `DeformationField`: the reusable result of a computation.

use std::borrow::Cow;
use std::path::PathBuf;

use pyo3::exceptions::{PyIndexError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyBytes, PyTuple};
use pyo3::{intern, IntoPyObjectExt};
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

    /// Projects points. An array-like (anything with `__array__`: numpy array, DataFrame…)
    /// of shape `(n, 2)` gives an `(n, 2)` float64 numpy array; any other iterable of
    /// `(x, y)` pairs gives a list of tuples.
    fn project_points<'py>(
        &self,
        py: Python<'py>,
        points: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if points.hasattr(intern!(py, "__array__"))? {
            return self.project_array(py, points);
        }
        let coords = points
            .try_iter()?
            .enumerate()
            .map(|(k, item)| point_from_py(&item?, k))
            .collect::<PyResult<Vec<Coord<f64>>>>()?;
        let projected: Vec<(f64, f64)> = py.detach(|| {
            coords
                .iter()
                .map(|&c| {
                    let c = self.inner.project_coord(c);
                    (c.x, c.y)
                })
                .collect()
        });
        projected.into_bound_py_any(py)
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

impl PyField {
    /// Projects an `(n, 2)` array-like through its raw float64 bytes, so a large lattice
    /// costs no Python object per point. PyO3's buffer API would avoid the `tobytes` copy,
    /// but it needs the stable ABI of Python 3.11.
    fn project_array<'py>(
        &self,
        py: Python<'py>,
        points: &Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let numpy = py.import("numpy")?;
        let float64 = numpy.getattr("float64")?;
        let array = numpy.call_method1("ascontiguousarray", (points, &float64))?;
        let shape: Vec<usize> = array.getattr("shape")?.extract()?;
        if shape.len() != 2 || shape[1] != 2 {
            return Err(PyValueError::new_err(format!(
                "expected an array of shape (n, 2), got {shape:?}"
            )));
        }
        let bytes = array.call_method0("tobytes")?;
        let bytes = bytes.cast::<PyBytes>()?.as_bytes();
        let out: Vec<u8> = py.detach(|| {
            let mut out = Vec::with_capacity(bytes.len());
            for point in bytes.chunks_exact(16) {
                let (x, y) = point.split_at(8);
                let c = self.inner.project_coord(Coord {
                    x: f64::from_ne_bytes(x.try_into().expect("8 bytes")),
                    y: f64::from_ne_bytes(y.try_into().expect("8 bytes")),
                });
                out.extend_from_slice(&c.x.to_ne_bytes());
                out.extend_from_slice(&c.y.to_ne_bytes());
            }
            out
        });
        // A bytearray, unlike bytes, gives a writable array.
        numpy
            .call_method1("frombuffer", (PyByteArray::new(py, &out), float64))?
            .call_method1("reshape", (shape[0], 2))
    }
}

/// Reads one `(x, y)` pair: a tuple, or any sequence of two numbers.
fn point_from_py(item: &Bound<'_, PyAny>, index: usize) -> PyResult<Coord<f64>> {
    if let Ok((x, y)) = item.extract::<(f64, f64)>() {
        return Ok(Coord { x, y });
    }
    let pair = || -> PyResult<Coord<f64>> {
        if item.len()? != 2 {
            return Err(PyValueError::new_err("not a pair"));
        }
        Ok(Coord {
            x: item.get_item(0)?.extract()?,
            y: item.get_item(1)?.extract()?,
        })
    };
    pair().map_err(|_| {
        PyTypeError::new_err(format!("point {index}: expected an (x, y) pair of numbers"))
    })
}
