# scapetoad (Python)

Python bindings for the `scapetoad` Rust library: diffusion-based cartograms
(Gastner–Newman), with a deformation field that carries basemaps along.

## Build

Needs Rust and [maturin](https://www.maturin.rs). From this directory:

```sh
uv venv && uv pip install maturin pytest geopandas
maturin develop --release        # builds the extension into the active venv
pytest

maturin build --release          # wheel in ../../target/wheels (abi3: CPython ≥ 3.10)
maturin build --release --features gdal   # with GeoPackage, GeoParquet… (needs libgdal)
```

The crate is a workspace member but not a default member, so `cargo build` and `cargo test`
at the workspace root skip it and need no Python.

## Use

```python
import scapetoad as st

cantons = st.read_layer('testdata/scapetoad-sample/Cantons.shp')
lakes = st.read_layer('testdata/scapetoad-sample/Lakes.shp')
cantons.crs = st.Crs(epsg=21781)     # the sample has no .prj; this does not reproject

carto = st.compute(
    cantons, 'Pop2006',
    quality=70,                      # or nodes=1000
    include=[lakes],                 # basemap: the envelope must cover it
    progress=lambda p: print(p.stage, p.fraction),
)
print(carto.report)                  # ScapeToad's text report; fields are attributes too
print(carto.errors.mean)             # size error, 100 = exact area

carto.master.write('out/cantons.gpkg')                 # with a SizeError attribute
carto.field.transform_layer(lakes).write('out/lakes.fgb')
carto.field.grid(50).write('out/grid.geojson')
carto.legend().write('out/legend.geojson')
carto.field.save('out/cantons.stdf')

field = st.DeformationField.load('out/cantons.stdf')   # later, for more basemaps
```

`compute` runs without holding the GIL. Ctrl-C, or an exception raised by the `progress`
callback, stops it at the next diffusion step.

### geopandas

```python
gdf = geopandas.read_file('communes.gpkg').to_crs(2056)
carto = st.compute(st.from_geodataframe(gdf, name='communes'), 'POP')
result = st.to_geodataframe(carto.master)
rivers = st.transform_geodataframe(carto.field, geopandas.read_file('rivers.gpkg'))
```

`from_geodataframe` keeps the columns and the CRS, but not the index. Missing values become
nulls and datetimes become ISO strings, since layers hold only bool, int, float and text.
`transform_geodataframe` returns a copy with deformed geometries and keeps everything else.

Without geopandas, geometries go in and out as WKB: `Layer.from_wkb(wkb_list, columns)`,
`layer.to_wkb()`, `field.transform_wkb(wkb_list)`. Points go through `field.project(x, y)` or
`field.project_points(points)`.

## API

| Name | What it is |
|---|---|
| `read_layer`, `write_layer`, `write_layers` | I/O, format from the extension; `file.gpkg#layer` for multi-layer files, `write_layers(..., quantize=N)` for TopoJSON |
| `Layer` | geometries, attribute columns (`fields`, `column`, `columns`), `crs`, `envelope` |
| `compute` | the computation; see the docstring or `_scapetoad.pyi` for every option |
| `Cartogram` | `master`, `field`, `masses`, `size_errors`, `errors`, `report`, `legend()` |
| `DeformationField` | `load`/`save`, `transform_layer`, `transform_wkb`, `project`, `grid`; picklable |
| `Report`, `ErrorSummary`, `Progress` | computation report, size error statistics, callback argument |
| `ScapeToadError` | raised for library errors; I/O errors raise `OSError` |

FlatGeobuf output is written with a spatial index, which reorders the features. Use GeoJSON,
GeoPackage or Shapefile when the order matters (for example, to match `size_errors`).
