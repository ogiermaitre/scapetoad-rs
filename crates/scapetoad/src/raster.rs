//! Polygon rasterisation onto the cartogram cells.

use geo::{BoundingRect, Intersects};
use geo_types::{Coord, Geometry, Polygon, Rect};

/// The undeformed cells of the cartogram grid: `cnx × cny` cells starting at `origin`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cells {
    pub origin: Coord<f64>,
    pub cw: f64,
    pub ch: f64,
    pub cnx: usize,
    pub cny: usize,
}

impl Cells {
    pub fn len(&self) -> usize {
        self.cnx * self.cny
    }

    /// Calls `f(cell index)` for every cell whose centre lies inside `poly` (even-odd rule
    /// over all rings, so holes are excluded).
    ///
    /// Equivalent to ScapeToad's per-cell `geom.contains(centre)` test, but done by scanline:
    /// one pass per row instead of one point-in-polygon test per cell.
    pub fn for_each_cell_in_polygon(&self, poly: &Polygon<f64>, mut f: impl FnMut(usize)) {
        let Some(bbox) = poly.bounding_rect() else {
            return;
        };
        let row_lo = ((bbox.min().y - self.origin.y) / self.ch - 0.5)
            .ceil()
            .max(0.0);
        let row_hi = ((bbox.max().y - self.origin.y) / self.ch - 0.5)
            .floor()
            .min(self.cny as f64 - 1.0);
        // Also rejects NaN bounds from degenerate geometries.
        if row_lo.is_nan() || row_hi.is_nan() || row_lo > row_hi {
            return;
        }
        let mut xs: Vec<f64> = Vec::new();
        for j in row_lo as usize..=row_hi as usize {
            let yc = self.origin.y + (j as f64 + 0.5) * self.ch;
            xs.clear();
            for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
                for seg in ring.lines() {
                    let (p, q) = (seg.start, seg.end);
                    // Half-open rule so a vertex exactly on the scanline is counted once.
                    if (p.y <= yc) != (q.y <= yc) {
                        xs.push(p.x + (yc - p.y) * (q.x - p.x) / (q.y - p.y));
                    }
                }
            }
            xs.sort_unstable_by(f64::total_cmp);
            for pair in xs.chunks_exact(2) {
                let lo = ((pair[0] - self.origin.x) / self.cw - 0.5).ceil().max(0.0);
                let hi = ((pair[1] - self.origin.x) / self.cw - 0.5)
                    .ceil()
                    .min(self.cnx as f64)
                    - 1.0;
                if lo <= hi {
                    for i in lo as usize..=hi as usize {
                        f(j * self.cnx + i);
                    }
                }
            }
        }
    }

    /// Calls `f(cell index)` for every cell whose rectangle intersects `geom`.
    pub fn for_each_cell_touching(&self, geom: &Geometry<f64>, mut f: impl FnMut(usize)) {
        let Some(bbox) = geom.bounding_rect() else {
            return;
        };
        let clamp_i = |v: f64| (v.floor().max(0.0) as usize).min(self.cnx - 1);
        let clamp_j = |v: f64| (v.floor().max(0.0) as usize).min(self.cny - 1);
        let i0 = clamp_i((bbox.min().x - self.origin.x) / self.cw);
        let i1 = clamp_i((bbox.max().x - self.origin.x) / self.cw);
        let j0 = clamp_j((bbox.min().y - self.origin.y) / self.ch);
        let j1 = clamp_j((bbox.max().y - self.origin.y) / self.ch);
        for j in j0..=j1 {
            for i in i0..=i1 {
                let min = Coord {
                    x: self.origin.x + i as f64 * self.cw,
                    y: self.origin.y + j as f64 * self.ch,
                };
                let cell = Rect::new(
                    min,
                    Coord {
                        x: min.x + self.cw,
                        y: min.y + self.ch,
                    },
                );
                if cell.intersects(geom) {
                    f(j * self.cnx + i);
                }
            }
        }
    }
}

/// Polygons of a geometry, ignoring points and lines (they carry no area).
pub(crate) fn polygons(geom: &Geometry<f64>) -> Vec<&Polygon<f64>> {
    match geom {
        Geometry::Polygon(p) => vec![p],
        Geometry::MultiPolygon(mp) => mp.0.iter().collect(),
        Geometry::GeometryCollection(gc) => gc.iter().flat_map(polygons).collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Contains;
    use geo_types::{polygon, Point};

    #[test]
    fn scanline_matches_point_in_polygon() {
        let cells = Cells {
            origin: Coord { x: 0.0, y: 0.0 },
            cw: 1.0,
            ch: 1.0,
            cnx: 20,
            cny: 20,
        };
        let poly = polygon!(
            exterior: [(x: 1.3, y: 2.2), (x: 17.6, y: 1.1), (x: 15.2, y: 18.4), (x: 8.0, y: 9.0), (x: 2.1, y: 16.7)],
            interiors: [[(x: 10.2, y: 4.1), (x: 13.9, y: 4.6), (x: 12.0, y: 8.3)]],
        );
        let mut got = vec![false; cells.len()];
        cells.for_each_cell_in_polygon(&poly, |k| got[k] = true);
        for j in 0..20 {
            for i in 0..20 {
                let centre = Point::new(i as f64 + 0.5, j as f64 + 0.5);
                assert_eq!(got[j * 20 + i], poly.contains(&centre), "cell {i},{j}");
            }
        }
    }
}
