# Test data

| Directory | Contents | Formats |
|---|---|---|
| [`scapetoad-sample/`](scapetoad-sample/README.md) | ScapeToad's own sample: 27 cantons (with Liechtenstein) by 2006 population, 14 lakes; LV03, no `.prj` | Shapefile |
| [`ch/`](ch/README.md) | Build script for Swiss communes, districts and cantons by 2025 population, country outline, lakes; LV95 | Shapefile, GeoJSON, FlatGeobuf, GeoParquet, GeoPackage |

`scapetoad-sample/` is small and checks agreement with ScapeToad. The Swiss dataset is recent
official data at three levels of detail; it is about 20 MB, so git keeps only
`ch/build.py`, which downloads the OFS sources and writes the data to `data/ch/` (ignored).
