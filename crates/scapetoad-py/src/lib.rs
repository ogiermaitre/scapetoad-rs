//! Python bindings for the `scapetoad` cartogram library.
//!
//! The native module is `scapetoad._scapetoad`; the Python package `scapetoad` re-exports it
//! and adds the geopandas helpers (`python/scapetoad`). Geometries cross the boundary as WKB.

use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyValueError};
use pyo3::prelude::*;

mod cartogram;
mod field;
mod layer;

use cartogram::{PyCartogram, PyErrorSummary, PyProgress, PyReport};
use field::PyField;
use layer::{PyCrs, PyLayer};

create_exception!(
    scapetoad,
    ScapeToadError,
    PyException,
    "Error raised by the cartogram computation or a format backend."
);

/// Converts a library error: I/O errors become the matching `OSError` subclass
/// (`FileNotFoundError`…), an option out of range a `ValueError`, everything else a
/// `ScapeToadError`.
pub(crate) fn py_err(err: scapetoad::Error) -> PyErr {
    match err {
        scapetoad::Error::Io(e) => e.into(),
        e @ scapetoad::Error::InvalidOption(_) => PyValueError::new_err(e.to_string()),
        other => ScapeToadError::new_err(other.to_string()),
    }
}

/// Reads a layer. The format comes from the extension; `file.gpkg#roads` picks a layer in a
/// multi-layer source.
#[pyfunction]
fn read_layer(py: Python<'_>, source: PathBuf) -> PyResult<PyLayer> {
    let layer = py
        .detach(|| scapetoad::io::read_layer(&source))
        .map_err(py_err)?;
    Ok(PyLayer::from(layer))
}

/// Writes a layer, replacing an existing file (a GeoPackage gains or replaces the layer).
#[pyfunction]
fn write_layer(py: Python<'_>, layer: PyRef<'_, PyLayer>, target: PathBuf) -> PyResult<()> {
    let layer = &layer.inner;
    py.detach(|| scapetoad::io::write_layer(layer, &target))
        .map_err(py_err)
}

/// Writes several layers into one file. In TopoJSON they share their borders; `quantize`
/// rounds coordinates to an N×N grid over the extent.
#[pyfunction]
#[pyo3(signature = (layers, target, *, quantize = None))]
fn write_layers(
    py: Python<'_>,
    layers: Vec<PyRef<'_, PyLayer>>,
    target: PathBuf,
    quantize: Option<u32>,
) -> PyResult<()> {
    let layers: Vec<&scapetoad::Layer> = layers.iter().map(|l| &l.inner).collect();
    let options = scapetoad::io::WriteOptions {
        topojson_quantization: quantize,
    };
    py.detach(|| scapetoad::io::write_layers(&layers, &target, &options))
        .map_err(py_err)
}

#[pymodule]
fn _scapetoad(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("SIZE_ERROR_FIELD", scapetoad::SIZE_ERROR_FIELD)?;
    m.add("ScapeToadError", m.py().get_type::<ScapeToadError>())?;
    m.add_class::<PyCrs>()?;
    m.add_class::<PyLayer>()?;
    m.add_class::<PyField>()?;
    m.add_class::<PyCartogram>()?;
    m.add_class::<PyReport>()?;
    m.add_class::<PyErrorSummary>()?;
    m.add_class::<PyProgress>()?;
    m.add_function(wrap_pyfunction!(read_layer, m)?)?;
    m.add_function(wrap_pyfunction!(write_layer, m)?)?;
    m.add_function(wrap_pyfunction!(write_layers, m)?)?;
    m.add_function(wrap_pyfunction!(cartogram::compute, m)?)?;
    Ok(())
}
