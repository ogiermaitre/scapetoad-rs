//! Topology construction and decoding for TopoJSON.
//!
//! A topology stores every line once: polygons and lines reference *arcs*, and two features
//! sharing a border reference the same arc, one of them in reverse. This is what makes
//! TopoJSON compact, and what keeps shared borders identical when they are later simplified.
//!
//! [`Topology::build`] follows the method of the reference JavaScript implementation
//! (topojson-server): find the *junctions*, the points where lines stop running together;
//! cut every line and ring at its junctions; then store each distinct piece once.
//! Coordinates are matched exactly, so shared borders must have bit-identical vertices,
//! which quantization (rounding to an integer grid) also guarantees.
//!
//! The module depends only on `geo-types`, so it can be used (or extracted) on its own; the
//! JSON encoding lives in `io::topojson`.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon,
    Point, Polygon,
};

/// Reference to an arc: `i` for arc `i` as stored, `!i` (that is `-i - 1`) for arc `i`
/// traversed backwards, as in the TopoJSON specification.
pub type ArcRef = i64;

/// A position in topology coordinates: integers stored as `f64` when quantized, the original
/// coordinates otherwise.
pub type Position = [f64; 2];

/// Maps quantized positions back to coordinates: `x = qx * scale[0] + translate[0]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub scale: [f64; 2],
    pub translate: [f64; 2],
}

impl Transform {
    fn decode(&self, p: Position) -> Coord<f64> {
        Coord {
            x: p[0] * self.scale[0] + self.translate[0],
            y: p[1] * self.scale[1] + self.translate[1],
        }
    }

    fn encode(&self, c: Coord<f64>) -> Position {
        [
            ((c.x - self.translate[0]) / self.scale[0]).round(),
            ((c.y - self.translate[1]) / self.scale[1]).round(),
        ]
    }
}

/// A geometry whose lines and rings are lists of arc references. Points keep their
/// positions, since they have no lines to share.
#[derive(Debug, Clone, PartialEq)]
pub enum TopoGeometry {
    /// A feature without geometry (TopoJSON `"type": null`).
    Null,
    Point(Position),
    MultiPoint(Vec<Position>),
    LineString(Vec<ArcRef>),
    MultiLineString(Vec<Vec<ArcRef>>),
    /// Rings, exterior first.
    Polygon(Vec<Vec<ArcRef>>),
    MultiPolygon(Vec<Vec<Vec<ArcRef>>>),
    GeometryCollection(Vec<TopoGeometry>),
}

/// Named collections of geometries sharing one set of arcs.
#[derive(Debug, Clone, PartialEq)]
pub struct Topology {
    /// `[min x, min y, max x, max y]` of the input, in original coordinates.
    pub bbox: Option<[f64; 4]>,
    /// Present when positions are quantized.
    pub transform: Option<Transform>,
    /// Objects in insertion order: a name and one geometry per feature.
    pub objects: Vec<(String, Vec<TopoGeometry>)>,
    /// Arcs as absolute positions (delta encoding is a matter of serialization).
    pub arcs: Vec<Vec<Position>>,
}

/// Hashable identity of a position. `-0.0` and `0.0` are the same point.
type Key = [u64; 2];

fn key(p: Position) -> Key {
    [(p[0] + 0.0).to_bits(), (p[1] + 0.0).to_bits()]
}

impl Topology {
    /// Builds a topology from named objects, each a list of optional geometries (one per
    /// feature, `None` for features without geometry).
    ///
    /// With `quantization = Some(n)`, positions are rounded to an `n` × `n` grid spanning the
    /// bounding box (`n` ≥ 2). This loses precision but snaps nearly identical vertices
    /// together and makes the encoded file much smaller. With `None`, coordinates are kept
    /// exactly and only bit-identical vertices are shared.
    pub fn build(
        objects: &[(&str, Vec<Option<&Geometry<f64>>>)],
        quantization: Option<u32>,
    ) -> Topology {
        let mut bbox = None;
        for g in objects.iter().flat_map(|(_, geoms)| geoms.iter().flatten()) {
            extend_bbox(g, &mut bbox);
        }
        let transform = quantization.zip(bbox).map(|(n, [x0, y0, x1, y1])| {
            let n = f64::from(n.max(2) - 1);
            // A degenerate extent (a single point, a vertical line) still needs a usable scale.
            let scale = |lo: f64, hi: f64| if hi > lo { (hi - lo) / n } else { 1.0 };
            Transform {
                scale: [scale(x0, x1), scale(y0, y1)],
                translate: [x0, y0],
            }
        });

        // Extract: every line and ring becomes a path, and geometries refer to their paths by
        // index for now (each arc list holds a single path index until `resolve`).
        let mut extractor = Extractor {
            transform,
            paths: Vec::new(),
        };
        let mut objs: Vec<(String, Vec<TopoGeometry>)> = objects
            .iter()
            .map(|(name, geoms)| {
                let geoms = geoms
                    .iter()
                    .map(|g| g.map_or(TopoGeometry::Null, |g| extractor.geometry(g)))
                    .collect();
                (name.to_string(), geoms)
            })
            .collect();
        let paths = extractor.paths;

        let junctions = find_junctions(&paths);
        let mut arcs = ArcStore::default();
        let path_arcs: Vec<Vec<ArcRef>> = paths
            .iter()
            .map(|p| cut(p, &junctions, &mut arcs))
            .collect();

        for (_, geoms) in &mut objs {
            for g in geoms {
                resolve(g, &path_arcs);
            }
        }
        Topology {
            bbox,
            transform,
            objects: objs,
            arcs: arcs.arcs,
        }
    }

    /// Converts a geometry of this topology back to coordinates. `None` for a null
    /// geometry or one referencing arcs that do not exist.
    pub fn geometry(&self, g: &TopoGeometry) -> Option<Geometry<f64>> {
        let point = |p: &Position| match &self.transform {
            Some(t) => t.decode(*p),
            None => Coord { x: p[0], y: p[1] },
        };
        Some(match g {
            TopoGeometry::Null => return None,
            TopoGeometry::Point(p) => Geometry::Point(Point(point(p))),
            TopoGeometry::MultiPoint(ps) => {
                Geometry::MultiPoint(MultiPoint(ps.iter().map(|p| Point(point(p))).collect()))
            }
            TopoGeometry::LineString(refs) => Geometry::LineString(self.line(refs)?),
            TopoGeometry::MultiLineString(lines) => Geometry::MultiLineString(MultiLineString(
                lines.iter().map(|l| self.line(l)).collect::<Option<_>>()?,
            )),
            TopoGeometry::Polygon(rings) => Geometry::Polygon(self.polygon(rings)?),
            TopoGeometry::MultiPolygon(polys) => Geometry::MultiPolygon(MultiPolygon(
                polys
                    .iter()
                    .map(|p| self.polygon(p))
                    .collect::<Option<_>>()?,
            )),
            TopoGeometry::GeometryCollection(gs) => Geometry::GeometryCollection(
                GeometryCollection(gs.iter().filter_map(|g| self.geometry(g)).collect()),
            ),
        })
    }

    /// Concatenates arcs into one line; each arc after the first starts where the previous
    /// one ended, so its first position is skipped.
    fn line(&self, refs: &[ArcRef]) -> Option<LineString<f64>> {
        let mut out: Vec<Coord<f64>> = Vec::new();
        for &r in refs {
            let (idx, reversed) = if r < 0 { (!r, true) } else { (r, false) };
            let arc = self.arcs.get(usize::try_from(idx).ok()?)?;
            let decode = |p: &Position| match &self.transform {
                Some(t) => t.decode(*p),
                None => Coord { x: p[0], y: p[1] },
            };
            let skip = usize::from(!out.is_empty());
            if reversed {
                out.extend(arc.iter().rev().skip(skip).map(decode));
            } else {
                out.extend(arc.iter().skip(skip).map(decode));
            }
        }
        Some(LineString(out))
    }

    fn polygon(&self, rings: &[Vec<ArcRef>]) -> Option<Polygon<f64>> {
        let mut rings = rings.iter().map(|r| self.line(r));
        let exterior = rings.next()??;
        Some(Polygon::new(exterior, rings.collect::<Option<_>>()?))
    }
}

/// Grows `bbox` to include every coordinate of `g`.
fn extend_bbox(g: &Geometry<f64>, bbox: &mut Option<[f64; 4]>) {
    let mut add = |c: &Coord<f64>| {
        *bbox = Some(match *bbox {
            None => [c.x, c.y, c.x, c.y],
            Some([x0, y0, x1, y1]) => [x0.min(c.x), y0.min(c.y), x1.max(c.x), y1.max(c.y)],
        });
    };
    let mut ring = |ls: &LineString<f64>| ls.0.iter().for_each(&mut add);
    match g {
        Geometry::Point(p) => add(&p.0),
        Geometry::MultiPoint(mp) => mp.iter().for_each(|p| add(&p.0)),
        Geometry::Line(l) => {
            add(&l.start);
            add(&l.end);
        }
        Geometry::LineString(ls) => ring(ls),
        Geometry::MultiLineString(mls) => mls.iter().for_each(ring),
        Geometry::Polygon(p) => std::iter::once(p.exterior())
            .chain(p.interiors())
            .for_each(ring),
        Geometry::MultiPolygon(mp) => mp
            .iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
            .for_each(ring),
        Geometry::Rect(r) => {
            add(&r.min());
            add(&r.max());
        }
        Geometry::Triangle(t) => t.to_array().iter().for_each(add),
        Geometry::GeometryCollection(gc) => gc.iter().for_each(|g| extend_bbox(g, bbox)),
    }
}

/// A line or ring in topology coordinates. Rings are stored without their closing position.
struct Path {
    points: Vec<Position>,
    closed: bool,
}

struct Extractor {
    transform: Option<Transform>,
    paths: Vec<Path>,
}

impl Extractor {
    fn position(&self, c: Coord<f64>) -> Position {
        match &self.transform {
            Some(t) => t.encode(c),
            None => [c.x, c.y],
        }
    }

    /// Registers a line or ring and returns the placeholder arc list pointing to it.
    fn path(&mut self, ls: &LineString<f64>, closed: bool) -> Vec<ArcRef> {
        let mut points: Vec<Position> = ls.0.iter().map(|&c| self.position(c)).collect();
        let mut deduped = points.clone();
        // Quantization can collapse neighbouring vertices; repeated positions would create
        // zero-length segments and spurious junctions.
        deduped.dedup_by(|a, b| key(*a) == key(*b));
        if closed && deduped.len() > 1 && key(deduped[0]) == key(deduped[deduped.len() - 1]) {
            deduped.pop();
        }
        // Keep degenerate rings and lines as they are rather than reducing them to nothing.
        let minimum = if closed { 3 } else { 2 };
        if deduped.len() >= minimum {
            points = deduped;
        } else if closed && points.len() > 1 && key(points[0]) == key(points[points.len() - 1]) {
            points.pop();
        }
        self.paths.push(Path { points, closed });
        vec![(self.paths.len() - 1) as ArcRef]
    }

    fn polygon(&mut self, p: &Polygon<f64>) -> Vec<Vec<ArcRef>> {
        std::iter::once(p.exterior())
            .chain(p.interiors())
            .map(|r| self.path(r, true))
            .collect()
    }

    fn geometry(&mut self, g: &Geometry<f64>) -> TopoGeometry {
        match g {
            Geometry::Point(p) => TopoGeometry::Point(self.position(p.0)),
            Geometry::MultiPoint(mp) => {
                TopoGeometry::MultiPoint(mp.iter().map(|p| self.position(p.0)).collect())
            }
            Geometry::Line(l) => {
                TopoGeometry::LineString(self.path(&LineString(vec![l.start, l.end]), false))
            }
            Geometry::LineString(ls) => TopoGeometry::LineString(self.path(ls, false)),
            Geometry::MultiLineString(mls) => {
                TopoGeometry::MultiLineString(mls.iter().map(|l| self.path(l, false)).collect())
            }
            Geometry::Polygon(p) => TopoGeometry::Polygon(self.polygon(p)),
            Geometry::MultiPolygon(mp) => {
                TopoGeometry::MultiPolygon(mp.iter().map(|p| self.polygon(p)).collect())
            }
            Geometry::Rect(r) => TopoGeometry::Polygon(self.polygon(&r.to_polygon())),
            Geometry::Triangle(t) => TopoGeometry::Polygon(self.polygon(&t.to_polygon())),
            Geometry::GeometryCollection(gc) => {
                TopoGeometry::GeometryCollection(gc.iter().map(|g| self.geometry(g)).collect())
            }
        }
    }
}

/// What we know about a position after visiting it on one or more paths.
enum Visit {
    /// Seen with these neighbours (sorted, so a path running the other way matches).
    Neighbours(Option<Key>, Option<Key>),
    Junction,
}

/// Positions where paths stop running together: endpoints of open lines, and positions met
/// with different neighbours on different visits (where a shared border ends or three
/// features meet).
fn find_junctions(paths: &[Path]) -> HashMap<Key, ()> {
    let mut visits: HashMap<Key, Visit> = HashMap::new();
    for path in paths {
        let n = path.points.len();
        for (i, &p) in path.points.iter().enumerate() {
            let (prev, next) = if path.closed {
                (
                    Some(key(path.points[(i + n - 1) % n])),
                    Some(key(path.points[(i + 1) % n])),
                )
            } else {
                let prev = i.checked_sub(1).map(|j| key(path.points[j]));
                let next = path.points.get(i + 1).map(|&q| key(q));
                (prev, next)
            };
            let endpoint = !path.closed && (i == 0 || i == n - 1);
            let pair = if prev <= next {
                (prev, next)
            } else {
                (next, prev)
            };
            match visits.entry(key(p)) {
                Entry::Vacant(e) => {
                    e.insert(if endpoint {
                        Visit::Junction
                    } else {
                        Visit::Neighbours(pair.0, pair.1)
                    });
                }
                Entry::Occupied(mut e) => {
                    let same = matches!(e.get(), Visit::Neighbours(a, b) if (*a, *b) == pair);
                    if endpoint || !same {
                        e.insert(Visit::Junction);
                    }
                }
            }
        }
    }
    visits
        .into_iter()
        .filter(|(_, v)| matches!(v, Visit::Junction))
        .map(|(k, _)| (k, ()))
        .collect()
}

/// Distinct arcs, each stored once in a canonical direction.
#[derive(Default)]
struct ArcStore {
    arcs: Vec<Vec<Position>>,
    index: HashMap<Vec<Key>, usize>,
}

impl ArcStore {
    /// Returns the reference to `points`, adding the arc if it is new. An arc and its
    /// reverse are the same arc: the lexicographically smaller direction is stored.
    fn add(&mut self, points: Vec<Position>) -> ArcRef {
        let forward: Vec<Key> = points.iter().map(|&p| key(p)).collect();
        let backward: Vec<Key> = forward.iter().rev().copied().collect();
        let reversed = backward < forward;
        let (canonical, stored) = if reversed {
            (backward, points.into_iter().rev().collect())
        } else {
            (forward, points)
        };
        let idx = *self.index.entry(canonical).or_insert_with(|| {
            self.arcs.push(stored);
            self.arcs.len() - 1
        });
        if reversed {
            !(idx as ArcRef)
        } else {
            idx as ArcRef
        }
    }
}

/// Cuts a path at its junctions and returns the arcs that make it up, in order.
fn cut(path: &Path, junctions: &HashMap<Key, ()>, arcs: &mut ArcStore) -> Vec<ArcRef> {
    let pts = &path.points;
    if pts.is_empty() {
        return Vec::new();
    }
    let is_junction = |p: &Position| junctions.contains_key(&key(*p));

    // Rings are rotated to start at a junction, so they can be cut like open lines. A ring
    // without junctions (an island, or an enclave and the hole around it) is one closed arc.
    // Starting it at its smallest position makes identical rings give identical arcs, in
    // both directions, since a ring read backwards from that position starts there too.
    let sequence: Vec<Position> = if path.closed {
        let first_junction = pts.iter().position(is_junction);
        let start = first_junction.unwrap_or_else(|| smallest(pts));
        let mut s: Vec<Position> = pts[start..].iter().chain(&pts[..start]).copied().collect();
        s.push(s[0]);
        if first_junction.is_none() {
            return vec![arcs.add(s)];
        }
        s
    } else {
        pts.clone()
    };

    let mut refs = Vec::new();
    let mut start = 0;
    for i in 1..sequence.len() {
        if i == sequence.len() - 1 || is_junction(&sequence[i]) {
            refs.push(arcs.add(sequence[start..=i].to_vec()));
            start = i;
        }
    }
    if refs.is_empty() {
        // A single position: still one (degenerate) arc, so the geometry stays valid.
        refs.push(arcs.add(sequence));
    }
    refs
}

/// Index of the smallest position (by its key), the canonical start of a closed ring.
fn smallest(pts: &[Position]) -> usize {
    (0..pts.len()).min_by_key(|&i| key(pts[i])).unwrap_or(0)
}

/// Replaces each placeholder (a one-element list holding a path index) by the path's arcs.
fn resolve(g: &mut TopoGeometry, path_arcs: &[Vec<ArcRef>]) {
    let swap = |refs: &mut Vec<ArcRef>| *refs = path_arcs[refs[0] as usize].clone();
    match g {
        TopoGeometry::Null | TopoGeometry::Point(_) | TopoGeometry::MultiPoint(_) => {}
        TopoGeometry::LineString(refs) => swap(refs),
        TopoGeometry::MultiLineString(lines) | TopoGeometry::Polygon(lines) => {
            lines.iter_mut().for_each(swap)
        }
        TopoGeometry::MultiPolygon(polys) => polys.iter_mut().flatten().for_each(swap),
        TopoGeometry::GeometryCollection(gs) => gs.iter_mut().for_each(|g| resolve(g, path_arcs)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{line_string, polygon};

    fn build(geoms: &[Geometry<f64>], q: Option<u32>) -> Topology {
        Topology::build(&[("o", geoms.iter().map(Some).collect())], q)
    }

    /// Same ring up to its starting position.
    fn same_ring(a: &LineString<f64>, b: &LineString<f64>) -> bool {
        let (a, b) = (&a.0[..a.0.len() - 1], &b.0[..b.0.len() - 1]);
        a.len() == b.len()
            && (0..b.len()).any(|s| {
                a.iter()
                    .zip(b[s..].iter().chain(&b[..s]))
                    .all(|(p, q)| p == q)
            })
    }

    fn assert_round_trip(topo: &Topology, input: &[Geometry<f64>]) {
        for (g, original) in topo.objects[0].1.iter().zip(input) {
            match (topo.geometry(g).unwrap(), original) {
                (Geometry::Polygon(a), Geometry::Polygon(b)) => {
                    assert!(same_ring(a.exterior(), b.exterior()), "{a:?} != {b:?}");
                    for (ra, rb) in a.interiors().iter().zip(b.interiors()) {
                        assert!(same_ring(ra, rb));
                    }
                }
                (a, b) => assert_eq!(&a, b),
            }
        }
    }

    #[test]
    fn neighbours_share_their_border() {
        let left = Geometry::Polygon(
            polygon![(x: 0., y: 0.), (x: 1., y: 0.), (x: 1., y: 1.), (x: 0., y: 1.)],
        );
        let right = Geometry::Polygon(
            polygon![(x: 1., y: 0.), (x: 2., y: 0.), (x: 2., y: 1.), (x: 1., y: 1.)],
        );
        let input = [left, right];
        let topo = build(&input, None);
        // The shared side, plus the rest of each square.
        assert_eq!(topo.arcs.len(), 3);
        let TopoGeometry::Polygon(a) = &topo.objects[0].1[0] else {
            panic!()
        };
        let TopoGeometry::Polygon(b) = &topo.objects[0].1[1] else {
            panic!()
        };
        let shared: Vec<_> = a[0].iter().filter(|r| b[0].contains(&!**r)).collect();
        assert_eq!(
            shared.len(),
            1,
            "the shared side is used once each way: {a:?} {b:?}"
        );
        assert_round_trip(&topo, &input);
    }

    #[test]
    fn enclave_and_hole_are_one_arc() {
        let outer = Geometry::Polygon(polygon!(
            exterior: [(x: 0., y: 0.), (x: 4., y: 0.), (x: 4., y: 4.), (x: 0., y: 4.)],
            interiors: [[(x: 1., y: 1.), (x: 1., y: 2.), (x: 2., y: 2.), (x: 2., y: 1.)]],
        ));
        // The enclave starts elsewhere and runs the other way round.
        let enclave = Geometry::Polygon(
            polygon![(x: 2., y: 2.), (x: 2., y: 1.), (x: 1., y: 1.), (x: 1., y: 2.)],
        );
        let input = [outer, enclave];
        let topo = build(&input, None);
        assert_eq!(topo.arcs.len(), 2);
        assert_round_trip(&topo, &input);
    }

    #[test]
    fn lines_are_cut_where_they_meet() {
        let a = Geometry::LineString(
            line_string![(x: 0., y: 0.), (x: 1., y: 0.), (x: 2., y: 0.), (x: 3., y: 0.)],
        );
        let b = Geometry::LineString(
            line_string![(x: 1., y: 1.), (x: 1., y: 0.), (x: 2., y: 0.), (x: 2., y: 1.)],
        );
        let input = [a, b];
        let topo = build(&input, None);
        // Both lines are cut at (1, 0) and (2, 0); the middle segment is stored once.
        assert_eq!(topo.arcs.len(), 5);
        assert_round_trip(&topo, &input);
    }

    #[test]
    fn quantization_snaps_to_the_grid() {
        let input = [Geometry::Polygon(
            polygon![(x: 0., y: 0.), (x: 10., y: 0.), (x: 10., y: 10.), (x: 0., y: 10.)],
        )];
        let topo = build(&input, Some(11));
        assert_eq!(topo.transform.unwrap().scale, [1.0, 1.0]);
        assert!(topo
            .arcs
            .iter()
            .flatten()
            .all(|p| p[0].fract() == 0.0 && p[1].fract() == 0.0));
        assert_round_trip(&topo, &input);
    }

    #[test]
    fn null_geometries_are_kept() {
        let topo = Topology::build(&[("o", vec![None])], None);
        assert_eq!(topo.objects[0].1, vec![TopoGeometry::Null]);
        assert!(topo.geometry(&topo.objects[0].1[0]).is_none());
    }
}
