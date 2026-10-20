//! `scapetoad` command-line tool: compute diffusion cartograms and apply them to basemaps.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use scapetoad::geo_types::Rect;
use scapetoad::{
    io, AttributeKind, Cartogram, CartogramOptions, DeformationField, FieldType, GridResolution,
    Layer, Progress,
};

#[derive(Parser)]
#[command(
    name = "scapetoad",
    version,
    about = "Diffusion-based cartograms (Gastner–Newman), a port of ScapeToad"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// TopoJSON outputs: round positions to an N×N grid over the extent and delta-encode
    /// them (1000000 ≈ 0.35 m over Switzerland). Smaller files, but no longer exact.
    #[arg(long, global = true, value_name = "N", value_parser = clap::value_parser!(u32).range(2..))]
    quantize: Option<u32>,
}

#[derive(Subcommand)]
enum Command {
    /// Describe a layer: features, geometry types, extent, CRS and attributes.
    Info {
        /// Input layer (`file.gpkg#layer` for multi-layer sources).
        source: PathBuf,
    },
    /// Compute a cartogram from a polygon layer and an attribute.
    Compute(Box<ComputeArgs>),
    /// Deform layers (basemaps) with a saved deformation field.
    Apply(ApplyArgs),
    /// Export the deformation grid of a saved field as lines.
    Grid {
        /// Deformation field written by `compute --field`.
        #[arg(long)]
        field: PathBuf,
        /// Output layer.
        #[arg(short, long)]
        output: PathBuf,
        /// Approximate number of lines along the longer side.
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    /// Convert a layer between formats, without deforming it.
    Convert { source: PathBuf, output: PathBuf },
}

#[derive(Args)]
struct ComputeArgs {
    /// Polygon layer whose areas should follow the attribute.
    master: PathBuf,
    /// Attribute holding the values.
    #[arg(short, long)]
    attr: String,
    /// The attribute is already a density (value per unit area), not a count.
    #[arg(long)]
    density: bool,
    /// Transformation quality 0-100: 250 + 15·q grid nodes along the longer side.
    #[arg(short, long, default_value_t = 50, value_parser = clap::value_parser!(u8).range(0..=100), conflicts_with = "nodes")]
    quality: u8,
    /// Grid nodes along the longer side (overrides --quality).
    #[arg(long)]
    nodes: Option<usize>,
    /// Value marking missing data; replaced by the mean of the other values.
    #[arg(long, allow_negative_numbers = true)]
    missing: Option<f64>,
    /// Envelope margin on each side, as a fraction of the layers' extent.
    #[arg(long, default_value_t = 0.2)]
    margin: f64,
    /// Stop the diffusion after this many seconds.
    #[arg(long, default_value_t = 3 * 3600)]
    max_time: u64,
    /// Stop the diffusion once no grid node moves more than this many cells per iteration.
    #[arg(long, default_value_t = 1e-7)]
    tolerance: f64,

    /// Output for the transformed master layer [default: <out-dir>/<name>_cartogram.<ext>].
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Save the deformation field, to transform more layers later with `apply`.
    #[arg(long)]
    field: Option<PathBuf>,
    /// Layer to transform along with the master: `in.shp` or `in.shp=out.gpkg`. Repeatable.
    #[arg(short, long = "layer", value_name = "IN[=OUT]")]
    layers: Vec<String>,
    /// Layer that keeps its shape where the master layer does not cover it. Repeatable.
    #[arg(long = "constrain", value_name = "SOURCE")]
    constrained: Vec<PathBuf>,
    /// Directory for default output names.
    #[arg(long, default_value = ".")]
    out_dir: PathBuf,
    /// Extension for default output names (shp, geojson, fgb, gpkg…) [default: input's].
    #[arg(long)]
    format: Option<String>,
    /// Write the deformation grid as lines.
    #[arg(long, value_name = "OUT")]
    grid: Option<PathBuf>,
    /// Approximate number of grid lines along the longer side.
    #[arg(long, default_value_t = 100)]
    grid_lines: usize,
    /// Write legend squares.
    #[arg(long, value_name = "OUT")]
    legend: Option<PathBuf>,
    /// Values shown by the legend squares [default: powers of ten below the maximum, and the maximum].
    #[arg(long, value_delimiter = ',')]
    legend_values: Option<Vec<f64>>,
    /// Write the computation report to a file instead of stdout.
    #[arg(long)]
    report: Option<PathBuf>,
    /// No progress output.
    #[arg(long)]
    quiet: bool,
}

#[derive(Args)]
struct ApplyArgs {
    /// Deformation field written by `compute --field`.
    #[arg(long)]
    field: PathBuf,
    /// Layers to transform.
    #[arg(required = true)]
    sources: Vec<PathBuf>,
    /// Output path (only with a single input).
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Directory for default output names (<name>_cartogram.<ext>).
    #[arg(long, default_value = ".")]
    out_dir: PathBuf,
    /// Extension for default output names [default: input's].
    #[arg(long)]
    format: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut outputs = Outputs::new(io::WriteOptions {
        topojson_quantization: cli.quantize,
    });
    match cli.command {
        Command::Info { source } => return info(&source),
        Command::Compute(args) => compute(*args, &mut outputs)?,
        Command::Apply(args) => apply(args, &mut outputs)?,
        Command::Grid {
            field,
            output,
            lines,
        } => {
            let field = DeformationField::load(&field)
                .with_context(|| format!("reading {}", field.display()))?;
            outputs.write(field.grid_layer(lines), &output)?;
        }
        Command::Convert { source, output } => outputs.write(read(&source)?, &output)?,
    }
    outputs.finish()
}

fn read(source: &Path) -> Result<Layer> {
    let layer = io::read_layer(source).with_context(|| format!("reading {}", source.display()))?;
    let why = match &layer.crs {
        Some(crs) if crs.is_geographic() => Some("is in geographic coordinates (lat/lon)"),
        // TopoJSON, and shapefiles without .prj, often carry no CRS at all.
        None if layer.looks_geographic() => {
            Some("has no CRS, and its coordinates look like longitude/latitude")
        }
        _ => None,
    };
    if let Some(why) = why {
        eprintln!(
            "warning: {} {why}; cartogram areas are only meaningful in a projected, ideally equal-area, CRS",
            source.display()
        );
    }
    Ok(layer)
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Writes output layers. Most formats are written at once; layers aimed at the same
/// TopoJSON file wait for [`Outputs::finish`], because the file is written whole so that
/// its layers share their borders.
struct Outputs {
    options: io::WriteOptions,
    topojson: Vec<(PathBuf, Vec<Layer>)>,
}

impl Outputs {
    fn new(options: io::WriteOptions) -> Self {
        Self {
            options,
            topojson: Vec::new(),
        }
    }

    /// Writes `layer` to `target`, or keeps it for its TopoJSON file. The layer is named
    /// after `#name` in the target, or keeps its own name (made unique within the file).
    fn write(&mut self, mut layer: Layer, target: &Path) -> Result<()> {
        let (path, name) = match target.to_string_lossy().rsplit_once('#') {
            Some((p, n)) if !n.is_empty() => (PathBuf::from(p), Some(n.to_string())),
            _ => (target.to_path_buf(), None),
        };
        if io::Format::from_path(&path) != io::Format::TopoJson {
            create_parent(target)?;
            io::write_layer(&layer, target)
                .with_context(|| format!("writing {}", target.display()))?;
            eprintln!("wrote {}", target.display());
            return Ok(());
        }
        let layers = match self.topojson.iter_mut().find(|(p, _)| *p == path) {
            Some((_, layers)) => layers,
            None => {
                self.topojson.push((path, Vec::new()));
                &mut self.topojson.last_mut().expect("just pushed").1
            }
        };
        let base = name.unwrap_or_else(|| layer.name.clone());
        layer.name = base.clone();
        let mut n = 2;
        while layers.iter().any(|l| l.name == layer.name) {
            layer.name = format!("{base}_{n}");
            n += 1;
        }
        layers.push(layer);
        Ok(())
    }

    /// Writes the TopoJSON files collected so far.
    fn finish(self) -> Result<()> {
        for (path, layers) in &self.topojson {
            create_parent(path)?;
            let refs: Vec<&Layer> = layers.iter().collect();
            io::write_layers(&refs, path, &self.options)
                .with_context(|| format!("writing {}", path.display()))?;
            let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
            eprintln!("wrote {} ({})", path.display(), names.join(", "));
        }
        Ok(())
    }
}

/// `<out_dir>/<stem>_cartogram.<ext>`, the extension defaulting to the input's.
fn default_output(source: &Path, out_dir: &Path, format: Option<&str>) -> PathBuf {
    let path_str = source.to_string_lossy();
    let (path, layer) = match path_str.rsplit_once('#') {
        Some((p, l)) if !source.exists() => (PathBuf::from(p), Some(l.to_string())),
        _ => (source.to_path_buf(), None),
    };
    let stem = layer.unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "layer".into())
    });
    let ext = format
        .map(str::to_string)
        .or_else(|| path.extension().map(|e| e.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "geojson".into());
    out_dir.join(format!("{stem}_cartogram.{ext}"))
}

fn info(source: &Path) -> Result<()> {
    let layer = read(source)?;
    println!("Layer: {}", layer.name);
    println!("Features: {}", layer.features.len());
    let mut kinds: Vec<(&'static str, usize)> = Vec::new();
    for f in &layer.features {
        let k = match &f.geometry {
            None => "None",
            Some(g) => geometry_name(g),
        };
        match kinds.iter_mut().find(|(n, _)| *n == k) {
            Some((_, c)) => *c += 1,
            None => kinds.push((k, 1)),
        }
    }
    let kinds: Vec<String> = kinds.iter().map(|(k, c)| format!("{k} ({c})")).collect();
    println!("Geometries: {}", kinds.join(", "));
    if let Some(env) = layer.envelope() {
        println!(
            "Extent: {} {} - {} {}",
            env.min().x,
            env.min().y,
            env.max().x,
            env.max().y
        );
    }
    match &layer.crs {
        Some(crs) => {
            if let Some(code) = crs.epsg {
                println!("CRS: EPSG:{code}");
            }
            if let Some(wkt) = &crs.wkt {
                let short: String = wkt.chars().take(100).collect();
                println!("CRS WKT: {short}{}", if wkt.len() > 100 { "…" } else { "" });
            }
        }
        None => println!("CRS: unknown"),
    }
    println!("Attributes:");
    for (k, field) in layer.fields.iter().enumerate() {
        let kind = match field.kind {
            FieldType::Bool => "bool",
            FieldType::Int => "int",
            FieldType::Float => "float",
            FieldType::Text => "text",
        };
        let nums: Vec<f64> = layer
            .features
            .iter()
            .filter_map(|f| f.values[k].as_f64())
            .collect();
        if matches!(field.kind, FieldType::Int | FieldType::Float) && !nums.is_empty() {
            let min = nums.iter().copied().fold(f64::INFINITY, f64::min);
            let max = nums.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let sum: f64 = nums.iter().sum();
            println!(
                "  {:<16} {kind:<6} min {min}  max {max}  sum {sum}",
                field.name
            );
        } else {
            let sample = layer
                .features
                .first()
                .map(|f| f.values[k].to_string())
                .unwrap_or_default();
            println!("  {:<16} {kind:<6} e.g. {sample:?}", field.name);
        }
    }
    Ok(())
}

fn geometry_name(g: &scapetoad::geo_types::Geometry<f64>) -> &'static str {
    use scapetoad::geo_types::Geometry::*;
    match g {
        Point(_) => "Point",
        Line(_) => "Line",
        LineString(_) => "LineString",
        Polygon(_) => "Polygon",
        MultiPoint(_) => "MultiPoint",
        MultiLineString(_) => "MultiLineString",
        MultiPolygon(_) => "MultiPolygon",
        GeometryCollection(_) => "GeometryCollection",
        Rect(_) => "Rect",
        Triangle(_) => "Triangle",
    }
}

fn compute(args: ComputeArgs, outputs: &mut Outputs) -> Result<()> {
    let master = read(&args.master)?;
    let constrained: Vec<Layer> = args
        .constrained
        .iter()
        .map(|p| read(p))
        .collect::<Result<_>>()?;
    let extra: Vec<(PathBuf, PathBuf, Layer)> = args
        .layers
        .iter()
        .map(|spec| {
            let (input, output) = match spec.split_once('=') {
                Some((i, o)) => (PathBuf::from(i), PathBuf::from(o)),
                None => {
                    let i = PathBuf::from(spec);
                    let o = default_output(&i, &args.out_dir, args.format.as_deref());
                    (i, o)
                }
            };
            let layer = read(&input)?;
            Ok((input, output, layer))
        })
        .collect::<Result<_>>()?;

    let mut options = CartogramOptions::new(&args.attr);
    options.kind = if args.density {
        AttributeKind::Density
    } else {
        AttributeKind::Mass
    };
    options.resolution = match args.nodes {
        Some(n) => GridResolution::Nodes(n),
        None => GridResolution::Quality(args.quality),
    };
    options.missing_value = args.missing;
    options.margin = args.margin;
    options.diffusion.max_duration = Some(Duration::from_secs(args.max_time));
    options.diffusion.tolerance = args.tolerance;

    let mut builder = Cartogram::builder(&master, options);
    for layer in &constrained {
        builder = builder.constrain(layer);
    }
    for (_, _, layer) in &extra {
        builder = builder.include(layer);
    }
    let quiet = args.quiet;
    let carto = builder.run_with_progress(|p| {
        if quiet {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = match p {
            Progress::Rasterizing => write!(err, "Computing the density grid…"),
            Progress::Diffusing(s) => write!(
                err,
                "\r\x1b[2KDiffusion: {:>3.0}%  iteration {}  max displacement {:.2e} cells",
                100.0 * s.fraction,
                s.iteration,
                s.max_displacement
            ),
            Progress::Constraining => write!(err, "\nApplying constrained deformation…"),
            Progress::Projecting => writeln!(err, "\nProjecting layers…"),
        };
        let _ = err.flush();
    })?;

    let master_out = args
        .output
        .clone()
        .unwrap_or_else(|| default_output(&args.master, &args.out_dir, args.format.as_deref()));
    outputs.write(carto.master.clone(), &master_out)?;
    for (input, output, layer) in &extra {
        let out = carto.field.transform_layer(layer);
        outputs
            .write(out, output)
            .with_context(|| format!("transforming {}", input.display()))?;
    }
    if let Some(path) = &args.field {
        carto
            .field
            .save(path)
            .with_context(|| format!("writing {}", path.display()))?;
        eprintln!("wrote {}", path.display());
    }
    if let Some(path) = &args.grid {
        outputs.write(carto.field.grid_layer(args.grid_lines), path)?;
    }
    if let Some(path) = &args.legend {
        outputs.write(carto.legend_layer(args.legend_values.as_deref()), path)?;
    }
    match &args.report {
        Some(path) => {
            std::fs::write(path, carto.report.to_string())?;
            eprintln!("wrote {}", path.display());
        }
        None => println!("{}", carto.report),
    }
    Ok(())
}

fn contains(outer: &Rect<f64>, inner: &Rect<f64>) -> bool {
    inner.min().x >= outer.min().x
        && inner.min().y >= outer.min().y
        && inner.max().x <= outer.max().x
        && inner.max().y <= outer.max().y
}

fn apply(args: ApplyArgs, outputs: &mut Outputs) -> Result<()> {
    if args.output.is_some() && args.sources.len() > 1 {
        bail!("--output takes a single input; use --out-dir for several");
    }
    let field = DeformationField::load(&args.field)
        .with_context(|| format!("reading {}", args.field.display()))?;
    for source in &args.sources {
        let layer = read(source)?;
        if layer
            .envelope()
            .is_some_and(|env| !contains(&field.envelope(), &env))
        {
            eprintln!(
                "warning: {} extends beyond the deformation field; outside parts are extrapolated. Pass it to `compute --layer` to include it in the envelope.",
                source.display()
            );
        }
        let out = args
            .output
            .clone()
            .unwrap_or_else(|| default_output(source, &args.out_dir, args.format.as_deref()));
        outputs.write(field.transform_layer(&layer), &out)?;
    }
    Ok(())
}
