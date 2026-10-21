//! Polygon rasterisation onto the cartogram cells.

use geo::{BoundingRect, LinesIter};
use geo_types::{Coord, Geometry, LineString, Polygon};

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

    /// Calls `f(cell index)` for every cell whose (closed) rectangle intersects `geom`, some
    /// cells more than once.
    ///
    /// A cell meets a polygon if its centre is inside, or if the boundary passes through it:
    /// the first set comes from the scanline of [`Self::for_each_cell_in_polygon`], the
    /// second from walking each segment through the cells it crosses. This costs about as
    /// much as rasterising the polygon, where testing every cell of the bounding box against
    /// the whole geometry was quadratic.
    pub fn for_each_cell_touching(&self, geom: &Geometry<f64>, mut f: impl FnMut(usize)) {
        self.touching(geom, &mut f);
    }

    fn touching(&self, geom: &Geometry<f64>, f: &mut dyn FnMut(usize)) {
        let mut polygon = |p: &Polygon<f64>| {
            self.for_each_cell_in_polygon(p, &mut *f);
            for line in p.lines_iter() {
                self.for_each_cell_on_segment(line.start, line.end, &mut *f);
            }
        };
        match geom {
            Geometry::Polygon(p) => polygon(p),
            Geometry::MultiPolygon(mp) => mp.iter().for_each(polygon),
            Geometry::Rect(r) => polygon(&r.to_polygon()),
            Geometry::Triangle(t) => polygon(&t.to_polygon()),
            Geometry::GeometryCollection(gc) => gc.iter().for_each(|g| self.touching(g, f)),
            Geometry::Point(p) => self.mark(self.cell_coords(p.0), f),
            Geometry::MultiPoint(mp) => mp.iter().for_each(|p| self.mark(self.cell_coords(p.0), f)),
            Geometry::Line(l) => self.for_each_cell_on_segment(l.start, l.end, f),
            Geometry::LineString(ls) => ls
                .lines()
                .for_each(|l| self.for_each_cell_on_segment(l.start, l.end, f)),
            Geometry::MultiLineString(mls) => mls
                .iter()
                .flat_map(LineString::lines)
                .for_each(|l| self.for_each_cell_on_segment(l.start, l.end, f)),
        }
    }

    /// Position in cell units: cell `(i, j)` spans `[i, i + 1] × [j, j + 1]`.
    fn cell_coords(&self, c: Coord<f64>) -> (f64, f64) {
        (
            (c.x - self.origin.x) / self.cw,
            (c.y - self.origin.y) / self.ch,
        )
    }

    /// Calls `f` for the cell holding `(u, v)` (cell units), and for its neighbours when the
    /// point lies on their shared side, since rectangles are closed.
    fn mark(&self, (u, v): (f64, f64), f: &mut dyn FnMut(usize)) {
        let range = |w: f64, n: usize| {
            let hi = w.floor();
            let lo = if w == hi { hi - 1.0 } else { hi };
            (lo.max(0.0), hi.min(n as f64 - 1.0))
        };
        let ((i0, i1), (j0, j1)) = (range(u, self.cnx), range(v, self.cny));
        // Also rejects NaN, for which every comparison is false.
        if !(i0 <= i1 && j0 <= j1) {
            return;
        }
        for j in j0 as usize..=j1 as usize {
            for i in i0 as usize..=i1 as usize {
                f(j * self.cnx + i);
            }
        }
    }

    /// Calls `f` for every cell the segment `a -> b` passes through, its ends included.
    fn for_each_cell_on_segment(&self, a: Coord<f64>, b: Coord<f64>, f: &mut dyn FnMut(usize)) {
        let (p, q) = (self.cell_coords(a), self.cell_coords(b));
        let at = |t: f64| (p.0 + t * (q.0 - p.0), p.1 + t * (q.1 - p.1));
        // Where the segment crosses a grid line; between two crossings it stays in one cell,
        // the one holding the middle of that piece.
        let mut ts = vec![0.0, 1.0];
        for (s, e, n) in [(p.0, q.0, self.cnx), (p.1, q.1, self.cny)] {
            if s == e || !s.is_finite() || !e.is_finite() {
                continue;
            }
            let (lo, hi) = (s.min(e).ceil().max(0.0), s.max(e).floor().min(n as f64));
            if lo <= hi {
                ts.extend((lo as usize..=hi as usize).map(|k| (k as f64 - s) / (e - s)));
            }
        }
        ts.sort_unstable_by(f64::total_cmp);
        // The crossings themselves too: where the segment goes through a grid corner, it
        // touches the two cells diagonal to its path.
        for &t in &ts {
            self.mark(at(t), f);
        }
        for w in ts.windows(2) {
            self.mark(at(0.5 * (w[0] + w[1])), f);
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
    use geo::{Contains, Intersects};
    use geo_types::{polygon, Point, Rect};

    /// Deterministic pseudo-random numbers in [0, 1) (64-bit LCG), to avoid a dependency.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Star-shaped ring around `c`, so it never self-intersects: vertices at increasing
    /// angles, radii between `r0` and `r1`.
    fn star(rng: &mut Lcg, c: (f64, f64), r0: f64, r1: f64, n: usize) -> LineString<f64> {
        let mut pts: Vec<Coord<f64>> = (0..n)
            .map(|k| {
                let a = std::f64::consts::TAU * (k as f64 + 0.8 * rng.next()) / n as f64;
                let r = r0 + (r1 - r0) * rng.next();
                Coord {
                    x: c.0 + r * a.cos(),
                    y: c.1 + r * a.sin(),
                }
            })
            .collect();
        pts.push(pts[0]);
        LineString(pts)
    }

    #[test]
    fn touching_cells_match_rectangle_intersection() {
        let cells = Cells {
            origin: Coord { x: -1.0, y: -2.0 },
            cw: 0.7,
            ch: 0.5,
            cnx: 30,
            cny: 40,
        };
        // The definition: every cell whose closed rectangle intersects the geometry.
        let expected = |g: &Geometry<f64>| -> Vec<bool> {
            (0..cells.len())
                .map(|k| {
                    let (i, j) = (k % cells.cnx, k / cells.cnx);
                    let min = Coord {
                        x: cells.origin.x + i as f64 * cells.cw,
                        y: cells.origin.y + j as f64 * cells.ch,
                    };
                    let max = Coord {
                        x: min.x + cells.cw,
                        y: min.y + cells.ch,
                    };
                    Rect::new(min, max).intersects(g)
                })
                .collect()
        };
        // Polygons with a hole, and their outlines as lines, some reaching out of the grid.
        let mut rng = Lcg(42);
        let mut geoms = vec![Geometry::Point(Point::new(3.3, 4.1))];
        for _ in 0..20 {
            let c = (-1.0 + 21.0 * rng.next(), -2.0 + 20.0 * rng.next());
            let outer = star(&mut rng, c, 2.0, 6.0, 12);
            let hole = star(&mut rng, c, 0.3, 1.5, 7);
            geoms.push(Geometry::Polygon(Polygon::new(outer.clone(), vec![hole])));
            geoms.push(Geometry::LineString(outer));
        }
        for g in &geoms {
            let mut got = vec![false; cells.len()];
            cells.for_each_cell_touching(g, |k| got[k] = true);
            assert_eq!(got, expected(g), "{g:?}");
        }
    }

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
