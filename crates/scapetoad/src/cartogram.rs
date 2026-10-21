//! Cartogram computation: density grid, diffusion, constraints, quality report.
//! Port of ScapeToad's `Cartogram`, `CartogramGrid` and `CartogramLayer` logic.

use std::fmt;
use std::time::{Duration, Instant};

use geo::{Area, CoordsIter};
use geo_types::{Coord, Geometry, LineString, Polygon, Rect};

use crate::diffusion::{diffuse, DiffusionOptions, DiffusionStep, StopReason};
use crate::error::{Error, Result};
use crate::field::DeformationField;
use crate::layer::{union_rect, Feature, Field, FieldType, Layer, Value};
use crate::raster::{polygons, Cells};
use crate::stats::{size_errors, ErrorSummary};

/// Name of the attribute added to the transformed master layer.
pub const SIZE_ERROR_FIELD: &str = "SizeError";

/// Default [`CartogramOptions::zero_density`].
pub const ZERO_DENSITY: f64 = 0.1;

/// How the cartogram attribute relates to the polygons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AttributeKind {
    /// A quantity (population, GDP…): the polygon's area should become proportional to it.
    #[default]
    Mass,
    /// Already a density (quantity per unit of area).
    Density,
}

/// Resolution of the cartogram grid. The shorter side of the envelope gets proportionally
/// fewer nodes so cells stay square.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridResolution {
    /// ScapeToad's "transformation quality" slider, 0..=100: `250 + 15 · q` nodes along the
    /// longer side (250 to 1750).
    Quality(u8),
    /// Explicit number of nodes along the longer side.
    Nodes(usize),
}

impl Default for GridResolution {
    fn default() -> Self {
        GridResolution::Quality(50)
    }
}

/// Parameters of a cartogram computation.
#[derive(Debug, Clone, PartialEq)]
pub struct CartogramOptions {
    /// Attribute of the master layer holding the values.
    pub attribute: String,
    pub kind: AttributeKind,
    pub resolution: GridResolution,
    /// Value marking missing data; such features get the mean of the other values.
    pub missing_value: Option<f64>,
    /// Margin added on each side of the layers' bounding box, as a fraction of its size
    /// (≥ 0). The map can only deform inside this envelope.
    pub margin: f64,
    /// Densities are rescaled so the smallest non-zero cell is at least this value (≥ 0)...
    pub min_density: f64,
    /// ...then cells at zero get this fraction of the smallest non-zero density (0 to 1).
    /// With the bias alone, they sit about 10⁶ times below their neighbours: the velocity
    /// `∇ρ / ρ` is huge at their edge and the adaptive step shrinks until the diffusion
    /// stalls. 0 keeps ScapeToad's behaviour. Data without zeros is not affected.
    pub zero_density: f64,
    /// ...and this is added to every cell (> 0), so zero-valued regions shrink but never
    /// vanish; without it, the velocity `∇ρ / ρ` is undefined where the density is zero.
    pub bias: f64,
    pub diffusion: DiffusionOptions,
}

impl CartogramOptions {
    pub fn new(attribute: impl Into<String>) -> Self {
        Self {
            attribute: attribute.into(),
            kind: AttributeKind::default(),
            resolution: GridResolution::default(),
            missing_value: None,
            margin: 0.2,
            min_density: 10.0,
            zero_density: ZERO_DENSITY,
            bias: 1e-5,
            diffusion: DiffusionOptions::default(),
        }
    }

    /// Rejects values outside their range, which would otherwise give a meaningless map
    /// reported as converged. Called before any computation.
    fn validate(&self) -> Result<()> {
        let at_least_zero = [
            ("margin", self.margin),
            ("min_density", self.min_density),
            ("tolerance", self.diffusion.tolerance),
        ];
        for (name, v) in at_least_zero {
            if !(v.is_finite() && v >= 0.0) {
                return Err(Error::InvalidOption(format!(
                    "{name} must be a finite number ≥ 0, got {v}"
                )));
            }
        }
        if !(0.0..=1.0).contains(&self.zero_density) {
            return Err(Error::InvalidOption(format!(
                "zero_density must be between 0 and 1, got {}",
                self.zero_density
            )));
        }
        if !(self.bias.is_finite() && self.bias > 0.0) {
            return Err(Error::InvalidOption(format!(
                "bias must be a finite number > 0, got {}",
                self.bias
            )));
        }
        Ok(())
    }
}

/// Progress of a computation, passed to the callback of [`CartogramBuilder::run_with_progress`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Progress {
    Rasterizing,
    Diffusing(DiffusionStep),
    Constraining,
    Projecting,
}

/// Configures and runs a cartogram computation.
///
/// ```no_run
/// # fn main() -> scapetoad::Result<()> {
/// use scapetoad::{io, Cartogram, CartogramOptions};
/// let cantons = io::read_layer("Cantons.shp")?;
/// let lakes = io::read_layer("Lakes.shp")?;
/// let carto = Cartogram::builder(&cantons, CartogramOptions::new("Pop2006"))
///     .include(&lakes)
///     .run()?;
/// io::write_layer(&carto.master, "cantons_cartogram.gpkg")?;
/// io::write_layer(&carto.field.transform_layer(&lakes), "lakes_cartogram.fgb")?;
/// # Ok(()) }
/// ```
pub struct CartogramBuilder<'a> {
    master: &'a Layer,
    options: CartogramOptions,
    constrained: Vec<&'a Layer>,
    included: Vec<&'a Layer>,
}

impl<'a> CartogramBuilder<'a> {
    /// Adds a layer whose features should keep their shape where the master layer does not
    /// cover them (ScapeToad's "constrained deformation" layers).
    pub fn constrain(mut self, layer: &'a Layer) -> Self {
        self.constrained.push(layer);
        self
    }

    /// Extends the envelope to cover a layer that will be transformed with the field later
    /// (basemaps). Outside the envelope, points are only extrapolated.
    pub fn include(mut self, layer: &'a Layer) -> Self {
        self.included.push(layer);
        self
    }

    pub fn run(self) -> Result<Cartogram> {
        self.run_with_progress(|_| {})
    }

    pub fn run_with_progress(self, mut progress: impl FnMut(Progress)) -> Result<Cartogram> {
        compute(self, &mut progress)
    }
}

/// Result of a cartogram computation.
#[derive(Debug, Clone)]
pub struct Cartogram {
    /// The deformation, reusable on any layer inside its envelope.
    pub field: DeformationField,
    /// Transformed master layer, with a [`SIZE_ERROR_FIELD`] attribute.
    pub master: Layer,
    /// Mass of each master feature (the value, or density × area).
    pub masses: Vec<f64>,
    /// Size error of each master feature, in percent (see [`crate::stats::size_errors`]).
    pub size_errors: Vec<f64>,
    /// Statistics of `size_errors`, without the zero-valued features ([`Report::zero_value`]).
    pub errors: ErrorSummary,
    pub report: Report,
}

impl Cartogram {
    pub fn builder(master: &Layer, options: CartogramOptions) -> CartogramBuilder<'_> {
        CartogramBuilder {
            master,
            options,
            constrained: Vec::new(),
            included: Vec::new(),
        }
    }

    /// Legend squares whose area represents the given masses at the cartogram's scale.
    ///
    /// Without `values`, uses ScapeToad's defaults: the two powers of ten below the maximum
    /// and the maximum itself. Squares are placed in a row under the envelope's lower-left
    /// corner. The scale comes from the transformed areas, which is what readers compare
    /// against (ScapeToad used the original areas).
    pub fn legend_layer(&self, values: Option<&[f64]>) -> Layer {
        let max = self.masses.iter().copied().fold(0.0, f64::max);
        let defaults;
        let values = match values {
            Some(v) => v,
            None => {
                let e = max.log10().floor();
                defaults = [10f64.powf(e - 1.0), 10f64.powf(e), max];
                &defaults[..]
            }
        };
        let total_area: f64 = self
            .master
            .features
            .iter()
            .filter_map(|f| f.geometry.as_ref())
            .map(|g| g.unsigned_area())
            .sum();
        let total_mass: f64 = self.masses.iter().sum();
        let env = self.field.envelope();
        let gap = env.width() / 30.0;

        let mut layer = Layer::new(
            "Legend",
            vec![
                Field::new("ID", FieldType::Int),
                Field::new("VALUE", FieldType::Float),
                Field::new("AREA", FieldType::Float),
            ],
        );
        layer.crs = self.field.crs.clone();
        let (mut x, y) = (env.min().x, env.min().y - gap);
        for (id, &value) in values.iter().enumerate() {
            let area = total_area / total_mass * value;
            let side = area.max(0.0).sqrt();
            let ring = LineString::from(vec![
                (x, y),
                (x + side, y),
                (x + side, y - side),
                (x, y - side),
                (x, y),
            ]);
            layer.features.push(Feature {
                geometry: Some(Geometry::Polygon(Polygon::new(ring, vec![]))),
                values: vec![
                    Value::Int(id as i64 + 1),
                    Value::Float(value),
                    Value::Float(area),
                ],
            });
            x += side + gap;
        }
        layer
    }
}

/// Parameters and outcome of a computation, printed like ScapeToad's computation report.
#[derive(Debug, Clone)]
pub struct Report {
    pub layer: String,
    pub attribute: String,
    pub kind: AttributeKind,
    pub grid_size: (usize, usize),
    pub zero_density: f64,
    pub bias: f64,
    pub feature_count: usize,
    /// Indices of the master features whose attribute holds no usable number (null, text
    /// that does not parse, NaN or infinite). They count as 0, as in ScapeToad.
    pub no_value: Vec<usize>,
    pub value_mean: f64,
    pub value_min: f64,
    pub value_max: f64,
    pub constrained_layers: Vec<String>,
    pub included_layers: Vec<String>,
    pub iterations: usize,
    pub stop: StopReason,
    /// Size error statistics, without the [`Report::zero_value`] features.
    pub errors: ErrorSummary,
    /// Indices of the master features whose mass is 0 (a zero, negative or missing value).
    /// Their size error is 0 whatever their area, so they are left out of `errors`…
    pub zero_value: Vec<usize>,
    /// …and their share of the master layer's area, in percent, before and after the
    /// transformation shows how far they shrank instead.
    pub zero_value_area: (f64, f64),
    /// Indices of the master features with an invalid geometry (OGC rules) before the
    /// transformation…
    pub invalid_input: Vec<usize>,
    /// …and after. A feature valid in input becomes invalid only where the grid folded.
    pub invalid_output: Vec<usize>,
    pub duration: Duration,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |names: &[String]| {
            if names.is_empty() {
                "None".to_string()
            } else {
                names.join("\n")
            }
        };
        writeln!(f, "CARTOGRAM COMPUTATION REPORT\n")?;
        writeln!(f, "CARTOGRAM PARAMETERS:")?;
        writeln!(f, "Cartogram layer: {}", self.layer)?;
        writeln!(f, "Cartogram attribute: {}", self.attribute)?;
        let kind = match self.kind {
            AttributeKind::Mass => "Population value",
            AttributeKind::Density => "Density value",
        };
        writeln!(f, "Attribute type: {kind}")?;
        writeln!(
            f,
            "Cartogram grid size: {} x {}",
            self.grid_size.0, self.grid_size.1
        )?;
        writeln!(
            f,
            "Zero-value density: {} of the smallest non-zero density",
            self.zero_density
        )?;
        writeln!(f, "Bias value: {}\n", self.bias)?;
        writeln!(f, "CARTOGRAM LAYER & ATTRIBUTE STATISTICS:")?;
        writeln!(f, "Number of features: {}", self.feature_count)?;
        if !self.no_value.is_empty() {
            writeln!(
                f,
                "Features without a numeric value, counted as 0: {} (feature index from 0: {})",
                self.no_value.len(),
                index_list(&self.no_value)
            )?;
        }
        writeln!(f, "Attribute mean value: {}", self.value_mean)?;
        writeln!(f, "Attribute minimum value: {}", self.value_min)?;
        writeln!(f, "Attribute maximum value: {}\n", self.value_max)?;
        writeln!(
            f,
            "SIMULTANEOUSLY TRANSFORMED LAYERS:\n{}\n",
            list(&self.included_layers)
        )?;
        writeln!(
            f,
            "CONSTRAINED DEFORMATION LAYERS:\n{}\n",
            list(&self.constrained_layers)
        )?;
        writeln!(f, "DIFFUSION:")?;
        writeln!(f, "Iterations: {} ({:?})\n", self.iterations, self.stop)?;
        let e = &self.errors;
        writeln!(f, "CARTOGRAM ERROR")?;
        writeln!(
            f,
            "The cartogram error is a measure for the quality of the result."
        )?;
        writeln!(f, "Mean cartogram error: {}", e.mean)?;
        writeln!(f, "Standard deviation: {}", e.std_dev)?;
        writeln!(f, "25th percentile: {}", e.p25)?;
        writeln!(f, "50th percentile: {}", e.p50)?;
        writeln!(f, "75th percentile: {}", e.p75)?;
        let pct = if e.count > 0 {
            (100.0 * e.within_one_std_dev as f64 / e.count as f64).round()
        } else {
            0.0
        };
        writeln!(
            f,
            "Features with mean error +/- 1 standard deviation: {} of {} ({pct}%)\n",
            e.within_one_std_dev, e.count
        )?;
        if !self.zero_value.is_empty() {
            writeln!(
                f,
                "Features with a zero value, left out of the error statistics: {} (feature index from 0: {})",
                self.zero_value.len(),
                index_list(&self.zero_value)
            )?;
            writeln!(
                f,
                "Their share of the total area: {:.2}% before, {:.2}% after\n",
                self.zero_value_area.0, self.zero_value_area.1
            )?;
        }
        writeln!(f, "GEOMETRY VALIDITY")?;
        writeln!(
            f,
            "Invalid geometries: {} in input, {} in output",
            self.invalid_input.len(),
            self.invalid_output.len()
        )?;
        for (when, list) in [
            ("input", &self.invalid_input),
            ("output", &self.invalid_output),
        ] {
            if !list.is_empty() {
                writeln!(
                    f,
                    "Invalid in {when} (feature index from 0): {}",
                    index_list(list)
                )?;
            }
        }
        writeln!(f)?;
        writeln!(
            f,
            "Computation time: {:.1} seconds",
            self.duration.as_secs_f64()
        )
    }
}

/// Comma-separated indices, cut after the first fifty so a broken run stays readable.
pub fn index_list(indices: &[usize]) -> String {
    const SHOWN: usize = 50;
    let mut s = indices
        .iter()
        .take(SHOWN)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if indices.len() > SHOWN {
        s.push_str(&format!(" and {} more", indices.len() - SHOWN));
    }
    s
}

/// Attribute values after cleaning: nulls, text that does not parse and non-finite values
/// become 0 (as in ScapeToad; [`Report::no_value`] lists them), and the missing value marker
/// becomes the mean of the other values.
fn clean_values(layer: &Layer, options: &CartogramOptions) -> Result<Vec<f64>> {
    let raw = layer.numeric_values(&options.attribute)?;
    if !layer.features.is_empty() && raw.iter().all(Option::is_none) {
        return Err(Error::AttributeNotNumeric(options.attribute.clone()));
    }
    let is_missing = |v: f64| options.missing_value.is_some_and(|m| v == m);
    let present: Vec<f64> = raw
        .iter()
        .flatten()
        .copied()
        .filter(|v| v.is_finite() && !is_missing(*v))
        .collect();
    let mean = if present.is_empty() {
        0.0
    } else {
        present.iter().sum::<f64>() / present.len() as f64
    };
    Ok(raw
        .into_iter()
        .map(|v| match v {
            Some(v) if is_missing(v) => mean,
            Some(v) if v.is_finite() => v,
            _ => 0.0,
        })
        .collect())
}

fn expand(env: Rect<f64>, margin: f64) -> Rect<f64> {
    let (dx, dy) = (env.width() * margin, env.height() * margin);
    Rect::new(
        Coord {
            x: env.min().x - dx,
            y: env.min().y - dy,
        },
        Coord {
            x: env.max().x + dx,
            y: env.max().y + dy,
        },
    )
}

/// Node counts for the envelope: the longer side gets the requested count.
fn grid_size(env: &Rect<f64>, resolution: GridResolution) -> (usize, usize) {
    let n = match resolution {
        GridResolution::Quality(q) => 250 + 15 * usize::from(q.min(100)),
        GridResolution::Nodes(n) => n,
    }
    .max(3);
    let (w, h) = (env.width(), env.height());
    let short = |ratio: f64| ((n as f64 * ratio).round() as usize).max(3);
    if w >= h {
        (n, short(h / w))
    } else {
        (short(w / h), n)
    }
}

fn compute(builder: CartogramBuilder<'_>, progress: &mut dyn FnMut(Progress)) -> Result<Cartogram> {
    let start = Instant::now();
    let CartogramBuilder {
        master,
        options,
        constrained,
        included,
    } = builder;
    options.validate()?;
    let non_finite = |g: &Geometry<f64>| {
        g.coords_iter()
            .any(|c| !(c.x.is_finite() && c.y.is_finite()))
    };
    if let Some(feature) = master
        .features
        .iter()
        .position(|f| f.geometry.as_ref().is_some_and(non_finite))
    {
        return Err(Error::NonFiniteCoordinate {
            layer: master.name.clone(),
            feature,
        });
    }

    // *** Values, areas and densities ***
    let values = clean_values(master, &options)?;
    let no_value = master.features_without_value(&options.attribute)?;
    let areas: Vec<f64> = master
        .features
        .iter()
        .map(|f| f.geometry.as_ref().map_or(0.0, |g| g.unsigned_area()))
        .collect();
    let total_area: f64 = areas.iter().sum();
    if total_area <= 0.0 {
        return Err(Error::NoPolygonArea);
    }
    let (densities, masses): (Vec<f64>, Vec<f64>) = values
        .iter()
        .zip(&areas)
        .map(|(&v, &a)| match options.kind {
            AttributeKind::Mass if a > 0.0 && v > 0.0 => (v / a, v),
            AttributeKind::Mass => (0.0, v.max(0.0)),
            AttributeKind::Density => (v.max(0.0), v.max(0.0) * a),
        })
        .unzip();
    if densities.iter().all(|&d| d <= 0.0) {
        return Err(Error::AllValuesZero(options.attribute.clone()));
    }
    let mean_density: f64 = densities
        .iter()
        .zip(&areas)
        .map(|(d, a)| d * a / total_area)
        .sum();

    // *** Envelope and grid ***
    let envelope = std::iter::once(master)
        .chain(constrained.iter().copied())
        .chain(included.iter().copied())
        .filter_map(Layer::envelope)
        .reduce(|a, b| union_rect(&a, &b))
        .ok_or(Error::NoPolygonArea)?;
    let envelope = expand(envelope, options.margin);
    let (nx, ny) = grid_size(&envelope, options.resolution);
    let mut field = DeformationField::identity(envelope, nx, ny)?;
    field.crs = master.crs.clone();
    let (cw, ch) = field.cell_size();
    let cells = Cells {
        origin: envelope.min(),
        cw,
        ch,
        cnx: nx - 1,
        cny: ny - 1,
    };

    // *** Density grid ***
    // Cells outside every polygon get the mean density, so the outside neither pushes nor pulls.
    progress(Progress::Rasterizing);
    let mut rho = vec![mean_density; cells.len()];
    // -1: empty, 0: covered by the master layer, 1: constrained.
    let mut flags = vec![-1i8; cells.len()];
    for (feat, &d) in master.features.iter().zip(&densities) {
        let Some(geom) = &feat.geometry else { continue };
        for poly in polygons(geom) {
            cells.for_each_cell_in_polygon(poly, |k| {
                rho[k] = d;
                flags[k] = 0;
            });
        }
    }
    let min_positive = rho
        .iter()
        .copied()
        .filter(|&v| v > 0.0)
        .fold(f64::INFINITY, f64::min);
    if min_positive.is_finite() {
        let factor = options.min_density / min_positive;
        if factor > 1.0 {
            rho.iter_mut().for_each(|v| *v *= factor);
        }
        let floor = options.zero_density * min_positive * factor.max(1.0);
        rho.iter_mut()
            .filter(|v| **v == 0.0)
            .for_each(|v| *v = floor);
    }
    rho.iter_mut().for_each(|v| *v += options.bias);

    for layer in &constrained {
        for geom in layer.features.iter().filter_map(|f| f.geometry.as_ref()) {
            cells.for_each_cell_touching(geom, |k| {
                if flags[k] == -1 {
                    flags[k] = 1;
                }
            });
        }
    }

    // *** Diffusion ***
    let result = diffuse(
        &rho,
        cells.cnx,
        cells.cny,
        &options.diffusion,
        &mut |step| progress(Progress::Diffusing(step)),
    );
    drop(rho);
    for j in 0..ny {
        for i in 0..nx {
            let k = j * nx + i;
            field.set_node(
                i,
                j,
                Coord {
                    x: envelope.min().x + result.xs[k] * cw,
                    y: envelope.min().y + result.ys[k] * ch,
                },
            );
        }
    }

    if !constrained.is_empty() {
        progress(Progress::Constraining);
        field.conform_to_constraints(&flags);
    }

    // *** Transformed master and quality ***
    progress(Progress::Projecting);
    let mut projected = field.transform_layer(master);
    let projected_areas: Vec<f64> = projected
        .features
        .iter()
        .map(|f| f.geometry.as_ref().map_or(0.0, |g| g.unsigned_area()))
        .collect();
    let errors_per_feature = size_errors(&masses, &projected_areas);
    let err_idx = projected.ensure_field(SIZE_ERROR_FIELD, FieldType::Float);
    for (feat, &e) in projected.features.iter_mut().zip(&errors_per_feature) {
        feat.values[err_idx] = Value::Float(e);
    }
    // A zero-valued feature's error is 0 whatever its area: counting it would pull the mean
    // towards 100 when nothing moved, so these features are reported apart.
    let zero_value: Vec<usize> = (0..masses.len()).filter(|&i| masses[i] == 0.0).collect();
    let counted: Vec<f64> = errors_per_feature
        .iter()
        .zip(&masses)
        .filter(|(_, &m)| m != 0.0)
        .map(|(&e, _)| e)
        .collect();
    let errors = ErrorSummary::from_values(&counted);
    let zero_share = |areas: &[f64]| {
        let zero: f64 = zero_value.iter().map(|&i| areas[i]).sum();
        100.0 * zero / areas.iter().sum::<f64>()
    };
    let zero_value_area = (zero_share(&areas), zero_share(&projected_areas));

    let value_mean = values.iter().sum::<f64>() / values.len().max(1) as f64;
    let report = Report {
        layer: master.name.clone(),
        attribute: options.attribute.clone(),
        kind: options.kind,
        grid_size: (nx, ny),
        zero_density: options.zero_density,
        bias: options.bias,
        feature_count: master.features.len(),
        no_value,
        value_mean,
        value_min: values.iter().copied().fold(f64::INFINITY, f64::min),
        value_max: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        constrained_layers: constrained.iter().map(|l| l.name.clone()).collect(),
        included_layers: included.iter().map(|l| l.name.clone()).collect(),
        iterations: result.iterations,
        stop: result.stop,
        errors,
        zero_value,
        zero_value_area,
        invalid_input: master.invalid_features(),
        invalid_output: projected.invalid_features(),
        duration: start.elapsed(),
    };

    Ok(Cartogram {
        field,
        master: projected,
        masses,
        size_errors: errors_per_feature,
        errors,
        report,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::polygon;

    /// Four unit squares in a 2×2 block; one holds 70% of the population.
    fn block() -> Layer {
        let mut layer = Layer::new("block", vec![Field::new("pop", FieldType::Int)]);
        let pops = [70, 10, 10, 10];
        for (k, pop) in pops.into_iter().enumerate() {
            let (x, y) = ((k % 2) as f64, (k / 2) as f64);
            let sq = polygon![(x: x, y: y), (x: x + 1.0, y: y), (x: x + 1.0, y: y + 1.0), (x: x, y: y + 1.0)];
            layer.features.push(Feature {
                geometry: Some(Geometry::Polygon(sq)),
                values: vec![Value::Int(pop)],
            });
        }
        layer
    }

    #[test]
    fn areas_move_towards_population_shares() {
        let layer = block();
        let mut options = CartogramOptions::new("pop");
        options.resolution = GridResolution::Nodes(129);
        let carto = Cartogram::builder(&layer, options).run().unwrap();
        assert_eq!(carto.report.stop, StopReason::Converged);
        let areas: Vec<f64> = carto
            .master
            .features
            .iter()
            .map(|f| f.geometry.as_ref().unwrap().unsigned_area())
            .collect();
        let share = areas[0] / areas.iter().sum::<f64>();
        // Started at 25%, target 70%; the sea around the block limits how far it can go.
        assert!(share > 0.55, "share {share}");
        assert!(carto.errors.mean > 80.0 && carto.errors.mean < 120.0);
    }

    /// Issue #10: with the bias alone, three zero-valued squares out of four stalled the
    /// diffusion, and the size error summary still looked perfect.
    #[test]
    fn zero_values_shrink_quickly_and_stay_out_of_the_error_summary() {
        let mut layer = block();
        for feat in &mut layer.features[1..] {
            feat.values[0] = Value::Int(0);
        }
        let mut options = CartogramOptions::new("pop");
        options.resolution = GridResolution::Nodes(129);
        let carto = Cartogram::builder(&layer, options).run().unwrap();
        let r = &carto.report;
        assert_eq!(r.stop, StopReason::Converged);
        assert!(r.iterations < 100, "{} iterations", r.iterations);
        assert!(r.invalid_output.is_empty());
        assert_eq!(r.zero_value, vec![1, 2, 3]);
        assert_eq!(r.errors.count, 1);
        assert_eq!(r.errors.mean, carto.size_errors[0]);
        let (before, after) = r.zero_value_area;
        assert!((before - 75.0).abs() < 1e-9, "{before}");
        assert!(after < 15.0, "{after}");
        let text = r.to_string();
        assert!(
            text.contains("left out of the error statistics: 3 (feature index from 0: 1, 2, 3)"),
            "{text}"
        );
    }

    #[test]
    fn zero_density_leaves_data_without_zeros_unchanged() {
        let layer = block();
        let run = |zero_density| {
            let mut options = CartogramOptions::new("pop");
            options.resolution = GridResolution::Nodes(65);
            options.zero_density = zero_density;
            Cartogram::builder(&layer, options).run().unwrap()
        };
        let (scapetoad, default) = (run(0.0), run(ZERO_DENSITY));
        assert_eq!(scapetoad.master, default.master);
        assert!(default.report.zero_value.is_empty());
    }

    #[test]
    fn grid_is_proportional_to_envelope() {
        let env = Rect::new((0.0, 0.0), (200.0, 100.0));
        assert_eq!(grid_size(&env, GridResolution::Quality(50)), (1000, 500));
        assert_eq!(grid_size(&env, GridResolution::Nodes(11)), (11, 6));
    }

    #[test]
    fn missing_values_get_the_mean() {
        let mut layer = block();
        layer.features[1].values[0] = Value::Int(-1);
        let mut options = CartogramOptions::new("pop");
        options.missing_value = Some(-1.0);
        let vals = clean_values(&layer, &options).unwrap();
        assert_eq!(vals, vec![70.0, 30.0, 10.0, 10.0]);
    }

    #[test]
    fn features_without_a_number_are_reported() {
        let mut layer = block();
        layer.fields[0].kind = FieldType::Text;
        let texts = ["70", "1'234", "12,5", "10"];
        for (feat, text) in layer.features.iter_mut().zip(texts) {
            feat.values[0] = Value::Text(text.into());
        }
        layer.features[3].values[0] = Value::Null;
        let mut options = CartogramOptions::new("pop");
        options.resolution = GridResolution::Nodes(33);
        let carto = Cartogram::builder(&layer, options).run().unwrap();
        assert_eq!(carto.report.no_value, vec![1, 2, 3]);
        assert_eq!(carto.masses, vec![70.0, 0.0, 0.0, 0.0]);
        let text = carto.report.to_string();
        assert!(
            text.contains(
                "Features without a numeric value, counted as 0: 3 (feature index from 0: 1, 2, 3)"
            ),
            "{text}"
        );
    }

    #[test]
    fn options_out_of_range_are_rejected() {
        let layer = block();
        let with = |set: fn(&mut CartogramOptions)| {
            let mut options = CartogramOptions::new("pop");
            set(&mut options);
            options
        };
        let cases = [
            ("margin", with(|o| o.margin = -0.6)),
            ("margin", with(|o| o.margin = f64::NAN)),
            ("min_density", with(|o| o.min_density = f64::INFINITY)),
            ("zero_density", with(|o| o.zero_density = -0.1)),
            ("zero_density", with(|o| o.zero_density = 1.5)),
            ("zero_density", with(|o| o.zero_density = f64::NAN)),
            ("bias", with(|o| o.bias = 0.0)),
            ("bias", with(|o| o.bias = -1e-5)),
            ("tolerance", with(|o| o.diffusion.tolerance = -1.0)),
        ];
        for (name, options) in cases {
            match Cartogram::builder(&layer, options).run() {
                Err(Error::InvalidOption(msg)) => assert!(msg.starts_with(name), "{msg}"),
                other => panic!("{name}: {other:?}"),
            }
        }
    }

    #[test]
    fn non_finite_coordinates_are_rejected() {
        let mut layer = block();
        layer.features[0].geometry = Some(Geometry::Polygon(
            polygon![(x: 0.0, y: 0.0), (x: f64::NAN, y: 0.0), (x: 1.0, y: 1.0)],
        ));
        let res = Cartogram::builder(&layer, CartogramOptions::new("pop")).run();
        assert!(
            matches!(res, Err(Error::NonFiniteCoordinate { feature: 0, .. })),
            "{:?}",
            res.map(|c| c.report)
        );
    }
}
