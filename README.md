# scapetoad-rs

A Rust port of [ScapeToad](https://github.com/christiankaiser/ScapeToad), the cartogram
tool: it resizes the polygons of a map so their areas become proportional to a variable
(population, votes, GDP…), using the Gastner–Newman diffusion algorithm.

- `crates/scapetoad`: the library.
- `crates/scapetoad-cli`: the `scapetoad` command.
- `crates/scapetoad-py`: Python bindings, built with maturin (see its [README](crates/scapetoad-py/README.md)).

The computation produces a **deformation field**: a deformed grid that can be saved and
applied later to any vector layer inside its envelope. Basemaps (roads, rivers, lakes,
labels) are transformed this way, with the same deformation as the cartogram.

## Command line

```sh
cargo install --path crates/scapetoad-cli     # add --features gdal for GeoPackage, GeoParquet…

# Look at a layer: attributes, extent, CRS
scapetoad info testdata/scapetoad-sample/Cantons.shp

# Compute the cartogram, transform the lakes with it, save the field for later
scapetoad compute testdata/scapetoad-sample/Cantons.shp --attr Pop2006 \
    --layer testdata/scapetoad-sample/Lakes.shp \
    --field cantons.stdf --grid grid.geojson --legend legend.geojson \
    --out-dir out --format fgb

# Later: deform more basemaps with the same field
scapetoad apply --field cantons.stdf rivers.fgb roads.fgb --out-dir out

# Deformation grid at another density; format conversion
scapetoad grid --field cantons.stdf -o grid50.geojson --lines 50
scapetoad convert testdata/scapetoad-sample/Cantons.shp cantons.geojson
```

`compute` prints ScapeToad's computation report (size error statistics, plus the invalid
geometries before and after, by feature index). Main options:

| Option | Meaning |
|---|---|
| `--density` | the attribute is already a density, not a count |
| `-q, --quality 0..100` | grid of `250 + 15·q` nodes along the longer side (default 50) |
| `--nodes N` | explicit grid size |
| `--constrain LAYER` | keep the shape of these features where the master layer doesn't cover them |
| `--missing V` | value marking missing data, replaced by the mean |
| `--layer IN[=OUT]` | transform a basemap too, and include it in the envelope |

Layers given with `--layer` extend the envelope. A basemap applied later with `apply` should
lie inside the envelope; points outside are only extrapolated, and `apply` warns about it.

## Library

```rust
use scapetoad::{io, Cartogram, CartogramOptions, GridResolution};

let cantons = io::read_layer("testdata/scapetoad-sample/Cantons.shp")?;
let lakes = io::read_layer("testdata/scapetoad-sample/Lakes.shp")?;

let mut options = CartogramOptions::new("Pop2006");
options.resolution = GridResolution::Quality(70);
let carto = Cartogram::builder(&cantons, options)
    .include(&lakes)            // basemap: enlarge the envelope to cover it
    .run_with_progress(|p| eprintln!("{p:?}"))?;

println!("{}", carto.report);
io::write_layer(&carto.master, "cantons.gpkg")?;          // with a SizeError attribute
io::write_layer(&carto.field.transform_layer(&lakes), "lakes.fgb")?;
carto.field.save("cantons.stdf")?;
```

Geometries are [`geo-types`](https://docs.rs/geo-types) (re-exported as
`scapetoad::geo_types`); `DeformationField::transform` works on a single geometry.

## Formats

| Format | Extension | Feature |
|---|---|---|
| Shapefile | `.shp` | `shp` (default) |
| GeoJSON | `.geojson`, `.json` | `geojson` (default) |
| FlatGeobuf | `.fgb` | `fgb` (default) |
| TopoJSON | `.topojson`; `.json` when reading | `topojson` (default) |
| GeoPackage, GeoParquet, GML, KML, CSV, FileGDB… | `.gpkg`, `.parquet`, … | `gdal` (needs libgdal) |

Multi-layer sources take a layer name after `#`: `data.gpkg#roads`. Writing to an existing
GeoPackage adds or replaces that layer.

Every format keeps the features in their input order, which the report's feature indices
rely on: FlatGeobuf is written without a spatial index, which would sort them.

A TopoJSON file stores each border once, even when it is shared by two features or by two
layers (a commune and its canton), so all its layers are written together. The command
collects every output aimed at the same `.topojson` file and writes it at the end:

```sh
T=out/ch.topojson
scapetoad compute communes.fgb --attr POP -o "$T#communes" \
    --layer "cantons.fgb=$T#cantons" --layer "lakes.fgb=$T#lakes" \
    --grid "$T#grid" --legend "$T#legend" --quantize 1000000
```

Coordinates are exact by default. `--quantize N` rounds them to an N×N grid over the extent
and delta-encodes them; on the Swiss communes example, 10⁶ (about 0.3 m) gives 3.3 MB
instead of 12.4 MB. In the library, `io::write_layers` writes several layers at once and
`topology::Topology` builds the topology itself; it depends only on `geo-types`. QGIS reads
TopoJSON (through GDAL) but cannot write it.

TopoJSON is often published as `.json`: a `.json` file is read as TopoJSON or GeoJSON
according to its content (written `.json` files are GeoJSON). TopoJSON has no standard way to
state its coordinate system, so a file from elsewhere usually has none; published ones are
mostly longitude/latitude, which the tool then warns about (see below).

GeoJSON output carries the legacy `crs` member when the layer has an EPSG code. A GeoJSON file
without it is taken as WGS 84 (RFC 7946) only if its coordinates fit longitude/latitude
bounds; otherwise its CRS is unknown.

The `gdal` feature needs GDAL and libclang at build time (`brew install gdal`; libclang
comes with the Xcode command-line tools). Bindings are generated for the installed GDAL
version. Tested with GDAL 3.13. GDAL 3.13 support in the `gdal` crate is not released
yet, so the workspace pins it to a git revision (see `Cargo.toml`).

GeoParquet output stores exterior rings counter-clockwise, as its specification requires,
so the vertex order differs from a shapefile's; the shapes are identical.

Cartograms need planar coordinates, ideally an equal-area projection. The tool warns on
lat/lon input, and on input without a CRS whose coordinates all fall within ±180/±90, but
does not reproject.

## Differences from ScapeToad

The algorithm and its defaults are the same (envelope +20%, grid sizing, density rescaling
and bias, Runge–Kutta diffusion, constrained deformation). These Java bugs are fixed:

- `CartogramGrid.projectPoint` scaled coordinates by `n / width` instead of `(n-1) / width`,
  so points took their deformation from up to one cell away, and extrapolated at the far edge.
- `projectLineSegment` incremented the wrong index (`i2y` for `i1y`).
- Constrained deformation read `x` instead of `y` for one corner, tested the wrong corner
  cell, checked neighbours against cell bounds instead of node bounds, and could index out
  of bounds at the grid border.
- Percentiles were computed over a `TreeSet`, dropping duplicate values.
- In density mode, the size error and legend summed densities as if they were masses.
- The missing-value mean included the missing-value markers themselves.

Other changes:

- Diffusion stops when no node moves more than `tolerance` (1e-7 cells) instead of exactly
  0, or as soon as the density spectrum shows that no node can move that far any more, with
  an iteration cap and a time limit (ScapeToad declared a 3-hour limit but never enforced
  it). The spectral test matters: once the density is flat, rounding noise in the transform
  (larger for grid sizes the DCT cannot factor, like 1999 cells) leaves a tiny velocity
  that the growing time step amplifies, and a displacement test alone then never ends while
  the nodes drift into a broken grid.
- Geometries are projected through the grid by splitting each cell into two triangles and
  mapping each affinely, with a vertex added at every grid line and cell-diagonal crossing,
  instead of ScapeToad's bilinear interpolation. Inside a cell, bilinear interpolation bends
  straight edges, so edges closer than a fraction of a cell could cross; the piecewise-affine
  map is one-to-one wherever the grid is not folded, so a valid polygon stays valid. Vertices
  within 1e-9 cells of the previous one are dropped, since rounding could turn such
  near-duplicates into self-intersections.
- The report lists the invalid features (by index) besides counting them, and `compute`
  warns up front when the master layer has invalid geometries.
- Polygons are rasterised by scanline instead of one point-in-polygon test per cell, and the
  diffusion runs in parallel. The Swiss cantons sample takes about 1 s at quality 50.
- Segment densification at grid-line crossings is kept; ScapeToad's separate
  "regularisation" step is dropped because it only added vertices the crossings already add.
- The legend uses the transformed areas for its scale.
- The diffusion-only `CartogramGastner` class (unused in ScapeToad 1.2) is not ported.

## Deformation field file (`.stdf`)

Little-endian binary: magic `STDFIELD`, `u32` version (1), `u32` nx, `u32` ny, 4×`f64`
envelope (min x, min y, max x, max y), `i32` EPSG code (0 = none), `u32` length + UTF-8 WKT,
then nx·ny `f64` x coordinates followed by nx·ny `f64` y coordinates, row by row from the
bottom-left node.

## License

GPL-2.0-or-later, like ScapeToad (this is a derivative work). Test data in `testdata/`
(see its README): ScapeToad's Swiss cantons sample under the Public Geodata License, and
Swiss boundaries and population from the OFS, free to use with attribution.
