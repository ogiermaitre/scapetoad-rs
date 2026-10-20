# ScapeToad sample: Swiss cantons, population 2006

The sample data shipped with ScapeToad (`data/swiss-cantons` in the Java repository), copied
unchanged. `crates/scapetoad/tests/swiss_cantons.rs` and the examples in the main README use
it.

| File | Features | Contents |
|---|---|---|
| `Cantons.shp` | 27 | the 26 cantons and Liechtenstein (`Pays = FL`, `Geocode = 0`); `Pop2006`, names in five languages |
| `Lakes.shp` | 14 | the main lakes, basemap |

Coordinates are Swiss LV03 (EPSG:21781), but there is no `.prj`: the layers have no CRS when
read. There is no `.cpg` either.

Licence: Public Geodata License 0.1, © 2008 361DEGRES, Christian Kaiser (`License.txt`,
ISO-8859-1). The licence asks for verbatim copies, so these files are not modified here.
