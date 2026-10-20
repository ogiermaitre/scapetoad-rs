//! Diffusion-based cartograms, a Rust port of [ScapeToad](https://github.com/christiankaiser/ScapeToad).
//!
//! A cartogram resizes the polygons of a map so their areas become proportional to a
//! variable (population, votes…), using the Gastner–Newman diffusion algorithm.
//!
//! The computation produces a [`DeformationField`]: a deformed grid that can be applied to
//! any vector layer inside its envelope, then or later. This is how basemaps (roads, rivers,
//! lakes, labels) are carried along with the cartogram.
//!
//! ```no_run
//! # fn main() -> scapetoad::Result<()> {
//! use scapetoad::{io, Cartogram, CartogramOptions};
//!
//! let cantons = io::read_layer("Cantons.shp")?;
//! let carto = Cartogram::builder(&cantons, CartogramOptions::new("Pop2006")).run()?;
//! println!("{}", carto.report);
//! io::write_layer(&carto.master, "cantons_cartogram.geojson")?;
//! carto.field.save("cantons.stdf")?;
//!
//! // Later: deform a basemap with the saved field.
//! let field = scapetoad::DeformationField::load("cantons.stdf")?;
//! let rivers = io::read_layer("rivers.fgb")?;
//! io::write_layer(&field.transform_layer(&rivers), "rivers_cartogram.fgb")?;
//! # Ok(()) }
//! ```

pub mod cartogram;
pub mod diffusion;
pub mod error;
pub mod field;
pub mod io;
pub mod layer;
mod raster;
pub mod stats;

pub use cartogram::{
    AttributeKind, Cartogram, CartogramBuilder, CartogramOptions, GridResolution, Progress, Report,
    SIZE_ERROR_FIELD,
};
pub use diffusion::{DiffusionOptions, DiffusionStep, StopReason};
pub use error::{Error, Result};
pub use field::DeformationField;
/// Re-exported so callers build geometries with the same version as the library.
pub use geo_types;
pub use layer::{Crs, Feature, Field, FieldType, Layer, Value};
