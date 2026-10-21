//! End-to-end checks on ScapeToad's sample data: Swiss cantons by 2006 population.

use std::path::{Path, PathBuf};

use scapetoad::geo_types::Geometry;
use scapetoad::{
    io, Cartogram, CartogramOptions, DeformationField, GridResolution, Layer, StopReason,
};

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/scapetoad-sample")
        .join(name)
}

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("scapetoad-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn reads_the_sample_shapefile() {
    let cantons = io::read_layer(data("Cantons.shp")).unwrap();
    assert_eq!(cantons.features.len(), 27);
    let pop: f64 = cantons
        .numeric_values("Pop2006")
        .unwrap()
        .iter()
        .flatten()
        .sum();
    assert_eq!(pop, 7_591_000.0);
    let names = cantons.field_index("Nom_fr").unwrap();
    let geneve = cantons
        .features
        .iter()
        .any(|f| f.values[names].to_string() == "Genève");
    assert!(geneve, "dbf text should be decoded with its code page");
}

#[test]
fn population_cartogram_is_accurate_and_valid() {
    let cantons = io::read_layer(data("Cantons.shp")).unwrap();
    let lakes = io::read_layer(data("Lakes.shp")).unwrap();
    let mut options = CartogramOptions::new("Pop2006");
    options.resolution = GridResolution::Quality(20);
    let carto = Cartogram::builder(&cantons, options)
        .include(&lakes)
        .run()
        .unwrap();

    let r = &carto.report;
    assert_eq!(r.stop, StopReason::Converged);
    assert!(
        (r.errors.mean - 100.0).abs() < 3.0,
        "mean error {}",
        r.errors.mean
    );
    assert!(r.errors.std_dev < 8.0, "std dev {}", r.errors.std_dev);
    assert_eq!(r.invalid_output, r.invalid_input);
    assert!(r.invalid_output.is_empty());

    // The lakes must stay inside the field and keep all their features.
    let moved = carto.field.transform_layer(&lakes);
    assert_eq!(moved.features.len(), lakes.features.len());

    // A saved field reproduces the same transformation.
    let path = tmp("cantons.stdf");
    carto.field.save(&path).unwrap();
    let reloaded = DeformationField::load(&path).unwrap();
    assert_eq!(reloaded.transform_layer(&lakes), moved);
}

/// Same schema and features, in the same order: every format must keep the input order, since
/// the report and the size errors refer to features by index.
fn same_content(a: &Layer, b: &Layer) {
    assert_eq!(a.features.len(), b.features.len());
    assert_eq!(
        a.fields.iter().map(|f| &f.name).collect::<Vec<_>>(),
        b.fields.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
    for (fa, fb) in a.features.iter().zip(&b.features) {
        assert_eq!(fa.values, fb.values);
        assert_eq!(
            fa.geometry.clone().map(single),
            fb.geometry.clone().map(single)
        );
    }
}

/// Normalises what formats may legitimately change: FlatGeobuf promotes mixed
/// Polygon/MultiPolygon layers to multi, and GeoParquet stores exterior rings
/// counter-clockwise where shapefiles store them clockwise.
fn single(g: Geometry<f64>) -> Geometry<f64> {
    use geo::orient::{Direction, Orient};
    match g {
        Geometry::MultiPolygon(mut mp) if mp.0.len() == 1 => {
            Geometry::Polygon(mp.0.remove(0).orient(Direction::Default))
        }
        Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(mp.orient(Direction::Default)),
        Geometry::Polygon(p) => Geometry::Polygon(p.orient(Direction::Default)),
        g => g,
    }
}

#[test]
fn formats_round_trip() {
    let cantons = io::read_layer(data("Cantons.shp")).unwrap();
    for ext in ["geojson", "fgb", "shp"] {
        let path = tmp(&format!("roundtrip.{ext}"));
        io::write_layer(&cantons, &path).unwrap();
        let back = io::read_layer(&path).unwrap();
        same_content(&cantons, &back);
        // The sample has no .prj: no format may invent a CRS for it.
        assert_eq!(back.crs, None, "{ext}");
    }
}

#[test]
fn geojson_assumes_wgs84_only_for_lonlat_coordinates() {
    use scapetoad::geo_types::{polygon, Geometry};
    use scapetoad::{Crs, Feature, Layer};
    let epsg = |code| {
        Some(Crs {
            epsg: Some(code),
            wkt: None,
        })
    };
    let round_trip = |layer: &Layer, name: &str| {
        let path = tmp(name);
        io::write_layer(layer, &path).unwrap();
        io::read_layer(&path).unwrap().crs
    };

    // Projected coordinates without a `crs` member: unknown, not WGS 84.
    let mut cantons = io::read_layer(data("Cantons.shp")).unwrap();
    assert_eq!(round_trip(&cantons, "lv03_no_crs.geojson"), None);
    // A known EPSG code travels in the `crs` member.
    cantons.crs = epsg(21781);
    assert_eq!(round_trip(&cantons, "lv03.geojson"), epsg(21781));
    // Longitude/latitude coordinates without a `crs` member: RFC 7946's WGS 84.
    let mut lonlat = Layer::new("lonlat", Vec::new());
    lonlat.features.push(Feature {
        geometry: Some(Geometry::Polygon(polygon![
            (x: 5.9, y: 45.8), (x: 10.5, y: 45.8), (x: 10.5, y: 47.8), (x: 5.9, y: 47.8)
        ])),
        values: Vec::new(),
    });
    assert_eq!(round_trip(&lonlat, "lonlat.geojson"), epsg(4326));
}

#[cfg(feature = "gdal")]
#[test]
fn gdal_formats_round_trip() {
    let cantons = io::read_layer(data("Cantons.shp")).unwrap();
    for ext in ["gpkg", "parquet"] {
        let path = tmp(&format!("roundtrip.{ext}"));
        let _ = std::fs::remove_file(&path);
        io::write_layer(&cantons, &path).unwrap();
        let back = io::read_layer(&path).unwrap();
        same_content(&cantons, &back);
    }
}

#[cfg(feature = "gdal")]
#[test]
fn geopackage_holds_several_layers() {
    let cantons = io::read_layer(data("Cantons.shp")).unwrap();
    let lakes = io::read_layer(data("Lakes.shp")).unwrap();
    let path = tmp("swiss.gpkg");
    let _ = std::fs::remove_file(&path);
    let target = |layer: &str| PathBuf::from(format!("{}#{layer}", path.display()));

    io::write_layer(&cantons, target("cantons")).unwrap();
    io::write_layer(&lakes, target("lakes")).unwrap();
    // Writing a layer again replaces it instead of appending or failing.
    io::write_layer(&lakes, target("lakes")).unwrap();

    same_content(&cantons, &io::read_layer(target("cantons")).unwrap());
    same_content(&lakes, &io::read_layer(target("lakes")).unwrap());
    // Without a layer name, a multi-layer file is ambiguous.
    assert!(io::read_layer(&path).is_err());
}

#[cfg(feature = "topojson")]
#[test]
fn topojson_is_recognised_in_json_files() {
    let cantons = io::read_layer(data("Cantons.shp")).unwrap();
    let lakes = io::read_layer(data("Lakes.shp")).unwrap();
    // Published TopoJSON is often named .json, the extension we read as GeoJSON.
    let path = tmp("swiss_topo.json");
    let topo = tmp("swiss_topo.topojson");
    io::write_layers(&[&cantons, &lakes], &topo, &io::WriteOptions::default()).unwrap();
    std::fs::copy(&topo, &path).unwrap();

    let back = io::read_layer(format!("{}#Lakes", path.display())).unwrap();
    assert_eq!(back.name, "Lakes");
    assert_eq!(back.features.len(), lakes.features.len());
    // GeoJSON in a .json file still reads as GeoJSON.
    let geojson = tmp("cantons.json");
    io::write_layer(&cantons, &geojson).unwrap();
    assert_eq!(
        io::read_layer(&geojson).unwrap().features.len(),
        cantons.features.len()
    );
}

#[test]
fn geographic_coordinates_are_recognised() {
    use scapetoad::geo_types::{polygon, Geometry};
    use scapetoad::{Feature, Layer};
    // The sample is in Swiss LV03 metres, far outside longitude/latitude bounds.
    assert!(!io::read_layer(data("Cantons.shp"))
        .unwrap()
        .looks_geographic());
    let mut lonlat = Layer::new("lonlat", Vec::new());
    lonlat.features.push(Feature {
        geometry: Some(Geometry::Polygon(polygon![
            (x: 5.9, y: 45.8), (x: 10.5, y: 45.8), (x: 10.5, y: 47.8), (x: 5.9, y: 47.8)
        ])),
        values: Vec::new(),
    });
    assert!(lonlat.looks_geographic());
}

/// Normalises what a topology may change on top of [`single`]: rings start at a junction
/// rather than at their original first vertex, and repeated vertices are dropped.
fn rings_from_min(g: Geometry<f64>) -> Geometry<f64> {
    use scapetoad::geo_types::{Coord, LineString, MultiPolygon, Polygon};
    let ring = |ls: &LineString<f64>| {
        let mut pts: Vec<Coord<f64>> = ls.0[..ls.0.len() - 1].to_vec();
        pts.dedup();
        let start = (0..pts.len())
            .min_by(|&a, &b| {
                (pts[a].x, pts[a].y)
                    .partial_cmp(&(pts[b].x, pts[b].y))
                    .unwrap()
            })
            .unwrap();
        pts.rotate_left(start);
        pts.push(pts[0]);
        LineString(pts)
    };
    let polygon = |p: &Polygon<f64>| {
        Polygon::new(ring(p.exterior()), p.interiors().iter().map(ring).collect())
    };
    match single(g) {
        Geometry::Polygon(p) => Geometry::Polygon(polygon(&p)),
        Geometry::MultiPolygon(mp) => {
            Geometry::MultiPolygon(MultiPolygon(mp.iter().map(polygon).collect()))
        }
        g => g,
    }
}

#[cfg(feature = "topojson")]
#[test]
fn topojson_holds_several_layers() {
    let cantons = io::read_layer(data("Cantons.shp")).unwrap();
    let lakes = io::read_layer(data("Lakes.shp")).unwrap();
    let path = tmp("swiss.topojson");
    io::write_layers(&[&cantons, &lakes], &path, &io::WriteOptions::default()).unwrap();

    for layer in [&cantons, &lakes] {
        let back = io::read_layer(format!("{}#{}", path.display(), layer.name)).unwrap();
        assert_eq!(back.name, layer.name);
        assert_eq!(back.fields.len(), layer.fields.len());
        assert_eq!(back.features.len(), layer.features.len());
        for (a, b) in layer.features.iter().zip(&back.features) {
            assert_eq!(a.values, b.values);
            assert_eq!(
                a.geometry.clone().map(rings_from_min),
                b.geometry.clone().map(rings_from_min)
            );
        }
    }
    // Without a layer name, a multi-layer file is ambiguous.
    assert!(io::read_layer(&path).is_err());
}
