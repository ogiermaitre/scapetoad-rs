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

    // The lakes must stay inside the field and keep all their features.
    let moved = carto.field.transform_layer(&lakes);
    assert_eq!(moved.features.len(), lakes.features.len());

    // A saved field reproduces the same transformation.
    let path = tmp("cantons.stdf");
    carto.field.save(&path).unwrap();
    let reloaded = DeformationField::load(&path).unwrap();
    assert_eq!(reloaded.transform_layer(&lakes), moved);
}

/// Same schema and features. FlatGeobuf reorders features along its spatial index, so the
/// features are compared in a canonical order.
fn same_content(a: &Layer, b: &Layer) {
    assert_eq!(a.features.len(), b.features.len());
    assert_eq!(
        a.fields.iter().map(|f| &f.name).collect::<Vec<_>>(),
        b.fields.iter().map(|f| &f.name).collect::<Vec<_>>()
    );
    let sorted = |l: &Layer| {
        let mut feats = l.features.clone();
        feats.sort_by_cached_key(|f| format!("{:?}", f.values));
        feats
    };
    for (fa, fb) in sorted(a).iter().zip(&sorted(b)) {
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
    }
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
