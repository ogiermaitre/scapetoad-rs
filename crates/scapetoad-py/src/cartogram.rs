//! `compute` and its results: `Cartogram`, `Report`, `ErrorSummary`, `Progress`.

use std::panic::{self, AssertUnwindSafe};
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use scapetoad::stats::ErrorSummary;
use scapetoad::{
    AttributeKind, Cartogram, CartogramOptions, GridResolution, Layer, Progress, Report, StopReason,
};

use crate::field::PyField;
use crate::layer::PyLayer;
use crate::py_err;

/// Size error statistics over the master features (100 = exact area).
#[pyclass(name = "ErrorSummary", module = "scapetoad", frozen, get_all)]
pub struct PyErrorSummary {
    count: usize,
    mean: f64,
    std_dev: f64,
    p25: f64,
    p50: f64,
    p75: f64,
    /// Features whose error is within one standard deviation of the mean.
    within_one_std_dev: usize,
}

impl From<ErrorSummary> for PyErrorSummary {
    fn from(e: ErrorSummary) -> Self {
        Self {
            count: e.count,
            mean: e.mean,
            std_dev: e.std_dev,
            p25: e.p25,
            p50: e.p50,
            p75: e.p75,
            within_one_std_dev: e.within_one_std_dev,
        }
    }
}

#[pymethods]
impl PyErrorSummary {
    fn __repr__(&self) -> String {
        format!(
            "ErrorSummary(count={}, mean={:.3}, std_dev={:.3}, p50={:.3})",
            self.count, self.mean, self.std_dev, self.p50
        )
    }
}

/// Parameters and outcome of a computation; `str(report)` is ScapeToad's text report.
#[pyclass(name = "Report", module = "scapetoad", frozen)]
pub struct PyReport {
    inner: Report,
}

#[pymethods]
impl PyReport {
    #[getter]
    fn layer(&self) -> &str {
        &self.inner.layer
    }

    #[getter]
    fn attribute(&self) -> &str {
        &self.inner.attribute
    }

    /// `"mass"` or `"density"`.
    #[getter]
    fn kind(&self) -> &'static str {
        match self.inner.kind {
            AttributeKind::Mass => "mass",
            AttributeKind::Density => "density",
        }
    }

    #[getter]
    fn grid_size(&self) -> (usize, usize) {
        self.inner.grid_size
    }

    #[getter]
    fn bias(&self) -> f64 {
        self.inner.bias
    }

    #[getter]
    fn feature_count(&self) -> usize {
        self.inner.feature_count
    }

    #[getter]
    fn value_mean(&self) -> f64 {
        self.inner.value_mean
    }

    #[getter]
    fn value_min(&self) -> f64 {
        self.inner.value_min
    }

    #[getter]
    fn value_max(&self) -> f64 {
        self.inner.value_max
    }

    #[getter]
    fn constrained_layers(&self) -> Vec<String> {
        self.inner.constrained_layers.clone()
    }

    #[getter]
    fn included_layers(&self) -> Vec<String> {
        self.inner.included_layers.clone()
    }

    #[getter]
    fn iterations(&self) -> usize {
        self.inner.iterations
    }

    /// `"converged"`, `"max_iterations"` or `"timeout"`.
    #[getter]
    fn stop(&self) -> &'static str {
        match self.inner.stop {
            StopReason::Converged => "converged",
            StopReason::MaxIterations => "max_iterations",
            StopReason::Timeout => "timeout",
        }
    }

    #[getter]
    fn errors(&self) -> PyErrorSummary {
        self.inner.errors.into()
    }

    /// Indices of the master features with an invalid geometry before the transformation…
    #[getter]
    fn invalid_input(&self) -> Vec<usize> {
        self.inner.invalid_input.clone()
    }

    /// …and after. A feature valid in input becomes invalid only where the grid folded.
    #[getter]
    fn invalid_output(&self) -> Vec<usize> {
        self.inner.invalid_output.clone()
    }

    /// Computation time in seconds.
    #[getter]
    fn duration(&self) -> f64 {
        self.inner.duration.as_secs_f64()
    }

    fn __str__(&self) -> String {
        self.inner.to_string()
    }

    fn __repr__(&self) -> String {
        format!(
            "Report(layer={:?}, attribute={:?}, iterations={}, stop={:?}, mean_error={:.3})",
            self.inner.layer,
            self.inner.attribute,
            self.inner.iterations,
            self.stop(),
            self.inner.errors.mean
        )
    }
}

/// Progress of a computation, passed to the `progress` callback of `compute`.
///
/// `stage` is `"rasterizing"`, `"diffusing"`, `"constraining"` or `"projecting"`; the other
/// attributes are only set while diffusing.
#[pyclass(name = "Progress", module = "scapetoad", frozen, get_all)]
pub struct PyProgress {
    stage: &'static str,
    iteration: Option<usize>,
    /// Diffusion time reached.
    time: Option<f64>,
    /// Largest displacement of a node during the step, in grid cells.
    max_displacement: Option<f64>,
    /// Rough completion estimate in [0, 1].
    fraction: Option<f64>,
}

impl From<Progress> for PyProgress {
    fn from(p: Progress) -> Self {
        let stage = |stage| Self {
            stage,
            iteration: None,
            time: None,
            max_displacement: None,
            fraction: None,
        };
        match p {
            Progress::Rasterizing => stage("rasterizing"),
            Progress::Constraining => stage("constraining"),
            Progress::Projecting => stage("projecting"),
            Progress::Diffusing(s) => Self {
                stage: "diffusing",
                iteration: Some(s.iteration),
                time: Some(s.time),
                max_displacement: Some(s.max_displacement),
                fraction: Some(s.fraction),
            },
        }
    }
}

#[pymethods]
impl PyProgress {
    fn __repr__(&self) -> String {
        match (self.iteration, self.fraction) {
            (Some(i), Some(f)) => {
                format!("Progress({:?}, iteration={i}, fraction={f:.3})", self.stage)
            }
            _ => format!("Progress({:?})", self.stage),
        }
    }
}

/// Result of `compute`.
#[pyclass(name = "Cartogram", module = "scapetoad", frozen)]
pub struct PyCartogram {
    master: Py<PyLayer>,
    field: Py<PyField>,
    masses: Vec<f64>,
    size_errors: Vec<f64>,
    errors: ErrorSummary,
    report: Py<PyReport>,
}

#[pymethods]
impl PyCartogram {
    /// Transformed master layer, with a `SizeError` attribute.
    #[getter]
    fn master(&self, py: Python<'_>) -> Py<PyLayer> {
        self.master.clone_ref(py)
    }

    /// The deformation, reusable on any layer inside its envelope.
    #[getter]
    fn field(&self, py: Python<'_>) -> Py<PyField> {
        self.field.clone_ref(py)
    }

    /// Mass of each master feature (the value, or density × area).
    #[getter]
    fn masses(&self) -> Vec<f64> {
        self.masses.clone()
    }

    /// Size error of each master feature: 100 × mass share / area share, 100 = exact.
    #[getter]
    fn size_errors(&self) -> Vec<f64> {
        self.size_errors.clone()
    }

    #[getter]
    fn errors(&self) -> PyErrorSummary {
        self.errors.into()
    }

    #[getter]
    fn report(&self, py: Python<'_>) -> Py<PyReport> {
        self.report.clone_ref(py)
    }

    /// Legend squares whose areas represent `values` at the cartogram's scale; by default
    /// the two powers of ten below the largest mass, and the largest mass.
    #[pyo3(signature = (values = None))]
    fn legend(&self, py: Python<'_>, values: Option<Vec<f64>>) -> PyLayer {
        // The library computes the legend from a whole `Cartogram`, whose parts now live in
        // separate Python objects; rebuilding one costs a copy, which a legend can afford.
        let carto = Cartogram {
            field: self.field.get().inner.clone(),
            master: self.master.borrow(py).inner.clone(),
            masses: self.masses.clone(),
            size_errors: self.size_errors.clone(),
            errors: self.errors,
            report: self.report.get().inner.clone(),
        };
        PyLayer::from(carto.legend_layer(values.as_deref()))
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let (nx, ny) = self.field.get().inner.size();
        format!(
            "Cartogram({:?}, {} features, grid {nx}x{ny}, mean size error {:.3})",
            self.master.borrow(py).inner.name,
            self.masses.len(),
            self.errors.mean
        )
    }
}

impl PyCartogram {
    fn new(py: Python<'_>, carto: Cartogram) -> PyResult<Self> {
        Ok(Self {
            master: Py::new(py, PyLayer::from(carto.master))?,
            field: Py::new(py, PyField { inner: carto.field })?,
            masses: carto.masses,
            size_errors: carto.size_errors,
            errors: carto.errors,
            report: Py::new(
                py,
                PyReport {
                    inner: carto.report,
                },
            )?,
        })
    }
}

/// Computes a cartogram of `master`, resizing its polygons in proportion to `attribute`.
///
/// Options left to `None` take the library defaults. The computation runs without holding
/// the GIL; Ctrl-C, or an exception raised by `progress`, stops it at the next progress step.
#[pyfunction]
#[pyo3(signature = (
    master,
    attribute,
    *,
    density = false,
    quality = None,
    nodes = None,
    missing_value = None,
    margin = None,
    min_density = None,
    bias = None,
    tolerance = None,
    max_iterations = None,
    max_duration = None,
    constrain = None,
    include = None,
    progress = None,
))]
#[allow(clippy::too_many_arguments)]
pub fn compute(
    py: Python<'_>,
    master: PyRef<'_, PyLayer>,
    attribute: String,
    density: bool,
    quality: Option<u8>,
    nodes: Option<usize>,
    missing_value: Option<f64>,
    margin: Option<f64>,
    min_density: Option<f64>,
    bias: Option<f64>,
    tolerance: Option<f64>,
    max_iterations: Option<usize>,
    max_duration: Option<f64>,
    constrain: Option<Vec<PyRef<'_, PyLayer>>>,
    include: Option<Vec<PyRef<'_, PyLayer>>>,
    progress: Option<Py<PyAny>>,
) -> PyResult<PyCartogram> {
    let mut options = CartogramOptions::new(attribute);
    if density {
        options.kind = AttributeKind::Density;
    }
    options.resolution = match (quality, nodes) {
        (Some(_), Some(_)) => {
            return Err(PyValueError::new_err(
                "give either quality or nodes, not both",
            ))
        }
        (Some(q), None) if q > 100 => {
            return Err(PyValueError::new_err("quality must be between 0 and 100"))
        }
        (Some(q), None) => GridResolution::Quality(q),
        (None, Some(n)) => GridResolution::Nodes(n),
        (None, None) => options.resolution,
    };
    options.missing_value = missing_value;
    options.margin = margin.unwrap_or(options.margin);
    options.min_density = min_density.unwrap_or(options.min_density);
    options.bias = bias.unwrap_or(options.bias);
    let diffusion = &mut options.diffusion;
    diffusion.tolerance = tolerance.unwrap_or(diffusion.tolerance);
    diffusion.max_iterations = max_iterations.unwrap_or(diffusion.max_iterations);
    match max_duration {
        None => {}
        Some(s) if s == f64::INFINITY => diffusion.max_duration = None,
        Some(s) if s >= 0.0 => diffusion.max_duration = Some(Duration::from_secs_f64(s)),
        Some(_) => {
            return Err(PyValueError::new_err(
                "max_duration must be a non-negative number of seconds",
            ))
        }
    }

    let master = &master.inner;
    let constrain: Vec<&Layer> = constrain.iter().flatten().map(|l| &l.inner).collect();
    let include: Vec<&Layer> = include.iter().flatten().map(|l| &l.inner).collect();
    let carto = py.detach(|| run(master, options, &constrain, &include, progress.as_ref()))?;
    PyCartogram::new(py, carto)
}

/// Panic payload that unwinds out of the computation when Python asks it to stop. The
/// library has no cancellation hook, but its progress callback runs on the calling thread
/// between steps, and `resume_unwind` leaves without running the panic hook.
struct Interrupted;

/// Runs the computation, reattaching to Python at each progress step to check for signals
/// and call `callback`.
fn run(
    master: &Layer,
    options: CartogramOptions,
    constrain: &[&Layer],
    include: &[&Layer],
    callback: Option<&Py<PyAny>>,
) -> PyResult<Cartogram> {
    let mut builder = Cartogram::builder(master, options);
    for layer in constrain {
        builder = builder.constrain(layer);
    }
    for layer in include {
        builder = builder.include(layer);
    }
    let mut interruption: Option<PyErr> = None;
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        builder.run_with_progress(|p| {
            let checked = Python::attach(|py| {
                py.check_signals()?;
                if let Some(callback) = callback {
                    callback.call1(py, (PyProgress::from(p),))?;
                }
                Ok::<_, PyErr>(())
            });
            if let Err(err) = checked {
                interruption = Some(err);
                panic::resume_unwind(Box::new(Interrupted));
            }
        })
    }));
    match outcome {
        Ok(result) => result.map_err(py_err),
        Err(payload) if payload.is::<Interrupted>() => {
            Err(interruption.expect("an interruption records its error"))
        }
        Err(payload) => panic::resume_unwind(payload),
    }
}
