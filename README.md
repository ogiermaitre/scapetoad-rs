# scapetoad-rs

A Rust port of [ScapeToad](https://github.com/christiankaiser/ScapeToad), the cartogram
tool: it resizes the polygons of a map so their areas become proportional to a variable
(population, votes, GDP…), using the Gastner–Newman diffusion algorithm.

- `crates/scapetoad`: the library.
- `crates/scapetoad-cli`: the `scapetoad` command.

The computation produces a **deformation field**: a deformed grid that can be saved and
applied later to any vector layer inside its envelope. Basemaps (roads, rivers, lakes,
labels) are transformed this way, with the same deformation as the cartogram.

## Command line

```sh
cargo install --path crates/scapetoad-cli     # add --features gdal for GeoPackage, GeoParquet…

# Look at a layer: attributes, extent, CRS
scapetoad info testdata/Cantons.shp

# Compute the cartogram, transform the lakes with it, save the field for later
scapetoad compute testdata/Cantons.shp --attr Pop2006 \
    --layer testdata/Lakes.shp \
    --field cantons.stdf --grid grid.geojson --legend legend.geojson \
    --out-dir out --format fgb

# Later: deform more basemaps with the same field
scapetoad apply --field cantons.stdf rivers.fgb roads.fgb --out-dir out

# Deformation grid at another density; format conversion
scapetoad grid --field cantons.stdf -o grid50.geojson --lines 50
scapetoad convert testdata/Cantons.shp cantons.geojson
```

`compute` prints ScapeToad's computation report (size error statistics, plus a count of
invalid geometries before and after). Main options:

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

let cantons = io::read_layer("testdata/Cantons.shp")?;
let lakes = io::read_layer("testdata/Lakes.shp")?;

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
| GeoPackage, GeoParquet, GML, KML, CSV, FileGDB… | `.gpkg`, `.parquet`, … | `gdal` (needs libgdal) |

Multi-layer sources take a layer name after `#`: `data.gpkg#roads`. Writing to an existing
GeoPackage adds or replaces that layer.

Cartograms need planar coordinates, ideally an equal-area projection. The tool warns on
lat/lon input but does not reproject.

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
  0, with an iteration cap and a time limit (ScapeToad declared a 3-hour limit but never
  enforced it). On the sample data both criteria give identical results.
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

GPL-2.0-or-later, like ScapeToad (this is a derivative work). Sample data in `testdata/`:
Swiss cantons, Public Geodata License (see `testdata/License.txt`).
