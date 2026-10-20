# Swiss test dataset: boundaries of 6.4.2025, population 2025

Communes, districts and cantons of Switzerland with their permanent resident population,
plus the country outline and the lakes as basemaps. LV95 coordinates (EPSG:2056).

The data (about 20 MB) is not in git. Build it into `data/ch/`, which git ignores:

```sh
python3 testdata/ch/build.py            # needs ogr2ogr with the Parquet driver
```

It downloads both sources, writes `ch.gpkg`, then converts it to the other formats. Paths below
are relative to `data/ch/`.

| Layer | Features | Use |
|---|---|---|
| `communes` | 2118 | master layer, fine level |
| `districts` | 143 | master layer, middle level |
| `cantons` | 26 | master layer, coarse level |
| `country` | 1 | basemap (`--layer`) |
| `lakes` | 22 | basemap (`--layer`) |

## Formats

The same layers in five formats, with the same features in the same order and coordinates at
full precision:

| Path | Format | Reader |
|---|---|---|
| `shp/<layer>.shp` | Shapefile, UTF-8 `.cpg` | default |
| `geojson/<layer>.geojson` | GeoJSON with a `crs` member (not RFC 7946, which requires WGS84) | default |
| `fgb/<layer>.fgb` | FlatGeobuf, no spatial index | default |
| `parquet/<layer>.parquet` | GeoParquet | `gdal` feature |
| `ch.gpkg#<layer>` | GeoPackage, all five layers in one file | `gdal` feature |

Shapefiles store polygons and multipolygons as one type, so their layers read back as a mix of
`Polygon` and `MultiPolygon`; the other formats give `MultiPolygon` everywhere.

```sh
scapetoad compute data/ch/fgb/communes.fgb --attr POP \
    --layer data/ch/fgb/lakes.fgb --layer data/ch/fgb/country.fgb --out-dir out
```

## Attributes

Master layers carry `POP` (permanent resident population), `POP_CH` (Swiss), `POP_FOR`
(foreign), `AREA_HA` (area in hectares, lakes included) and `DENSITY` (inhabitants per km²,
for `--density`). Keys: `GDENR` (OFS commune number), `BEZNR` (district), `KTNR` and `KTKZ`
(canton number and abbreviation). Names: `GDENAME`, `BEZNAME`, `KTNAME`, `SEENAME`.

## Contents and choices

- Population is joined on the OFS commune number. Both sources use the commune state of
  6.4.2025, so every populated commune matches a polygon.
- District and canton populations are sums of the communes; `build.py` checks them against
  the totals STAT-TAB publishes for those levels.
- Total: 9,127,125. The "Sans indication" row (asylum seekers with no commune) is left out.
- Three communal forests (`GARTE = 12`: Staatswald Galm and two Ticino condominiums) have no
  residents. They are kept with `POP = 0` so the map has no holes; they test the zero-value
  path.
- Liechtenstein, Büsingen (D) and Campione (I), present in the source layers, are removed.
- Commune polygons cover the lakes; `lakes` is only for display.

## Sources

- Boundaries: OFS, generalised administrative boundaries G1, snapshot of 6.4.2025
  ([data.geo.admin.ch](https://data.geo.admin.ch/browser/index.html#/collections/ch.bfs.historisierte-administrative_grenzen_g1)).
- Population: OFS, STATPOP, STAT-TAB table
  [px-x-0102010000_101](https://www.pxweb.bfs.admin.ch/pxweb/fr/px-x-0102010000_101/px-x-0102010000_101/px-x-0102010000_101.px/),
  year 2025, permanent resident population, totals over sex and age.

Licence: free use with attribution (source: OFS / BFS); commercial use requires the OFS's
permission.
