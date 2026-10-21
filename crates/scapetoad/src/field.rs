//! The deformation field: a regular grid of nodes over an envelope, each node displaced to
//! its cartogram position. Any geometry inside the envelope can be projected through it,
//! which is how basemaps are transformed after (or independently of) the computation.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon,
    Point, Polygon, Rect,
};

use crate::error::{Error, Result};
use crate::layer::{Crs, Feature, Field, FieldType, Layer, Value};

const MAGIC: &[u8; 8] = b"STDFIELD";
const FORMAT_VERSION: u32 = 1;

/// Distance, in grid cells, under which a vertex counts as a duplicate of the previous one.
const DUPLICATE_TOLERANCE: f64 = 1e-9;

fn sub(p: Coord<f64>, q: Coord<f64>) -> Coord<f64> {
    Coord {
        x: p.x - q.x,
        y: p.y - q.y,
    }
}

/// Regular grid of `nx * ny` nodes spanning `envelope`, with the deformed position of each node.
///
/// Node `(i, j)` starts at `envelope.min + (i * cell_w, j * cell_h)`; its deformed position is
/// stored at index `j * nx + i`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeformationField {
    envelope: Rect<f64>,
    nx: usize,
    ny: usize,
    xs: Vec<f64>,
    ys: Vec<f64>,
    /// CRS of the layers the field was computed from, copied to transformed outputs.
    pub crs: Option<Crs>,
}

impl DeformationField {
    /// Undeformed field: every node at its original position. The envelope must be finite,
    /// with a positive width and height.
    pub fn identity(envelope: Rect<f64>, nx: usize, ny: usize) -> Result<Self> {
        if nx < 3 || ny < 3 {
            return Err(Error::GridTooSmall(nx, ny));
        }
        check_envelope(&envelope)?;
        let n = nx
            .checked_mul(ny)
            .ok_or_else(|| Error::InvalidField(format!("a {nx}x{ny} grid is too large")))?;
        let mut field = Self {
            envelope,
            nx,
            ny,
            xs: vec![0.0; n],
            ys: vec![0.0; n],
            crs: None,
        };
        let (cw, ch) = field.cell_size();
        for j in 0..ny {
            for i in 0..nx {
                let k = j * nx + i;
                field.xs[k] = envelope.min().x + i as f64 * cw;
                field.ys[k] = envelope.min().y + j as f64 * ch;
            }
        }
        Ok(field)
    }

    pub fn envelope(&self) -> Rect<f64> {
        self.envelope
    }

    /// Number of nodes in x and y.
    pub fn size(&self) -> (usize, usize) {
        (self.nx, self.ny)
    }

    /// Size of one undeformed cell.
    pub fn cell_size(&self) -> (f64, f64) {
        (
            self.envelope.width() / (self.nx - 1) as f64,
            self.envelope.height() / (self.ny - 1) as f64,
        )
    }

    /// Deformed position of node `(i, j)`.
    pub fn node(&self, i: usize, j: usize) -> Coord<f64> {
        let k = j * self.nx + i;
        Coord {
            x: self.xs[k],
            y: self.ys[k],
        }
    }

    pub(crate) fn set_node(&mut self, i: usize, j: usize, c: Coord<f64>) {
        let k = j * self.nx + i;
        self.xs[k] = c.x;
        self.ys[k] = c.y;
    }

    /// Converts a coordinate to continuous grid units (node `i` sits at `i`).
    fn to_grid(&self, c: Coord<f64>) -> (f64, f64) {
        let (cw, ch) = self.cell_size();
        (
            (c.x - self.envelope.min().x) / cw,
            (c.y - self.envelope.min().y) / ch,
        )
    }

    /// Projects one coordinate through the deformed cell containing it.
    ///
    /// Each cell is split into two triangles along a diagonal (see [`Self::splits_along_ac`])
    /// and mapped affinely on each. Unlike ScapeToad's bilinear interpolation, this keeps
    /// straight segments straight inside a triangle, so as long as no triangle of the grid is
    /// folded, the map is one-to-one and cannot make a valid geometry self-intersect.
    ///
    /// Points outside the envelope are extrapolated from the nearest border cell. ScapeToad
    /// scaled by `n / width` instead of `(n - 1) / width` here, which sampled the wrong cell
    /// by up to one cell width; this uses the node spacing consistently.
    pub fn project_coord(&self, c: Coord<f64>) -> Coord<f64> {
        let (gx, gy) = self.to_grid(c);
        self.project_grid(gx, gy)
    }

    /// Cell holding grid position `(gx, gy)`; border cells extend beyond the envelope.
    fn cell_of(&self, gx: f64, gy: f64) -> (usize, usize) {
        (
            (gx.floor() as isize).clamp(0, self.nx as isize - 2) as usize,
            (gy.floor() as isize).clamp(0, self.ny as isize - 2) as usize,
        )
    }

    /// Whether cell `(i, j)`, with corners `a b c d` counter-clockwise from the lower left,
    /// is split along `a–c` (the default) rather than `b–d`. The other diagonal is used when
    /// it is the only one giving two unfolded triangles, which is the case of a non-convex
    /// but unfolded cell.
    fn splits_along_ac(&self, i: usize, j: usize) -> bool {
        let a = self.node(i, j);
        let b = self.node(i + 1, j);
        let c = self.node(i + 1, j + 1);
        let d = self.node(i, j + 1);
        let cross = |p: Coord<f64>, q: Coord<f64>, r: Coord<f64>| {
            (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x)
        };
        let ac_ok = cross(a, b, c) > 0.0 && cross(a, c, d) > 0.0;
        let bd_ok = cross(a, b, d) > 0.0 && cross(b, c, d) > 0.0;
        ac_ok || !bd_ok
    }

    fn project_grid(&self, gx: f64, gy: f64) -> Coord<f64> {
        let (i, j) = self.cell_of(gx, gy);
        let ti = gx - i as f64;
        let tj = gy - j as f64;
        let a = self.node(i, j);
        let b = self.node(i + 1, j);
        let c = self.node(i + 1, j + 1);
        let d = self.node(i, j + 1);
        // Origin and the images of the two local axes for the triangle holding the point.
        let (o, ex, ey, si, sj) = if self.splits_along_ac(i, j) {
            if ti >= tj {
                (a, sub(b, a), sub(c, b), ti, tj)
            } else {
                (a, sub(c, d), sub(d, a), ti, tj)
            }
        } else if ti + tj <= 1.0 {
            (a, sub(b, a), sub(d, a), ti, tj)
        } else {
            (c, sub(d, c), sub(b, c), 1.0 - ti, 1.0 - tj)
        };
        Coord {
            x: o.x + si * ex.x + sj * ey.x,
            y: o.y + si * ex.y + sj * ey.y,
        }
    }

    /// Projects a coordinate sequence, inserting a vertex wherever a segment crosses a grid
    /// line or a cell's diagonal, so the output follows the piecewise-affine map exactly.
    ///
    /// Vertices closer than [`DUPLICATE_TOLERANCE`] cells to the previous one are dropped:
    /// such near-duplicates (often left by GIS overlays) form zigzags a few ulps wide, which
    /// rounding in the projection can turn into self-intersections.
    pub fn project_coords(&self, coords: &[Coord<f64>]) -> Vec<Coord<f64>> {
        let mut out = Vec::with_capacity(coords.len() * 2);
        let Some(&first) = coords.first() else {
            return out;
        };
        let mut p = self.to_grid(first);
        out.push(self.project_grid(p.0, p.1));
        let mut ts: Vec<f64> = Vec::new();
        for (n, &c) in coords.iter().enumerate().skip(1) {
            let q = self.to_grid(c);
            let is_last = n + 1 == coords.len();
            if (q.0 - p.0).abs().max((q.1 - p.1).abs()) < DUPLICATE_TOLERANCE {
                if is_last && out.len() > 1 {
                    // Keep the exact last vertex so closed rings stay closed.
                    out.pop();
                    out.push(self.project_grid(q.0, q.1));
                }
                continue;
            }
            ts.clear();
            ts.push(0.0);
            crossings(p.0, q.0, self.nx, &mut ts);
            crossings(p.1, q.1, self.ny, &mut ts);
            ts.push(1.0);
            ts.sort_unstable_by(f64::total_cmp);
            ts.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
            // Dedup may have dropped 1 in favour of a crossing just before it.
            *ts.last_mut().expect("holds 0 and 1") = 1.0;
            let at = |t: f64| (p.0 + t * (q.0 - p.0), p.1 + t * (q.1 - p.1));
            for w in ts.windows(2) {
                let (t0, t1) = (w[0], w[1]);
                if let Some(t) = self.diagonal_crossing(at(t0), at(t1)) {
                    let t = t0 + t * (t1 - t0);
                    let (x, y) = at(t);
                    out.push(self.project_grid(x, y));
                }
                let (x, y) = if t1 == 1.0 { q } else { at(t1) };
                out.push(self.project_grid(x, y));
            }
            p = q;
        }
        out
    }

    /// Parameter in (0, 1) where the piece `u -> v`, which lies within a single cell, strictly
    /// crosses that cell's diagonal.
    fn diagonal_crossing(&self, u: (f64, f64), v: (f64, f64)) -> Option<f64> {
        let (i, j) = self.cell_of(0.5 * (u.0 + v.0), 0.5 * (u.1 + v.1));
        let ac = self.splits_along_ac(i, j);
        // Signed position relative to the diagonal, linear along the piece.
        let side = |(x, y): (f64, f64)| {
            let (ti, tj) = (x - i as f64, y - j as f64);
            if ac {
                ti - tj
            } else {
                ti + tj - 1.0
            }
        };
        let (s0, s1) = (side(u), side(v));
        ((s0 < 0.0 && s1 > 0.0) || (s0 > 0.0 && s1 < 0.0)).then(|| s0 / (s0 - s1))
    }

    fn project_line_string(&self, ls: &LineString<f64>) -> LineString<f64> {
        LineString::new(self.project_coords(&ls.0))
    }

    fn project_polygon(&self, p: &Polygon<f64>) -> Polygon<f64> {
        Polygon::new(
            self.project_line_string(p.exterior()),
            p.interiors()
                .iter()
                .map(|r| self.project_line_string(r))
                .collect(),
        )
    }

    /// Projects any geometry. `Line`, `Rect` and `Triangle` become `LineString`/`Polygon`
    /// because their images are no longer straight.
    pub fn transform(&self, geom: &Geometry<f64>) -> Geometry<f64> {
        match geom {
            Geometry::Point(p) => Geometry::Point(Point(self.project_coord(p.0))),
            Geometry::MultiPoint(mp) => Geometry::MultiPoint(MultiPoint(
                mp.iter().map(|p| Point(self.project_coord(p.0))).collect(),
            )),
            Geometry::Line(l) => {
                Geometry::LineString(LineString::new(self.project_coords(&[l.start, l.end])))
            }
            Geometry::LineString(ls) => Geometry::LineString(self.project_line_string(ls)),
            Geometry::MultiLineString(mls) => Geometry::MultiLineString(MultiLineString(
                mls.iter().map(|ls| self.project_line_string(ls)).collect(),
            )),
            Geometry::Polygon(p) => Geometry::Polygon(self.project_polygon(p)),
            Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(MultiPolygon(
                mp.iter().map(|p| self.project_polygon(p)).collect(),
            )),
            Geometry::Rect(r) => Geometry::Polygon(self.project_polygon(&r.to_polygon())),
            Geometry::Triangle(t) => Geometry::Polygon(self.project_polygon(&t.to_polygon())),
            Geometry::GeometryCollection(gc) => Geometry::GeometryCollection(GeometryCollection(
                gc.iter().map(|g| self.transform(g)).collect(),
            )),
        }
    }

    /// Transforms every feature of a layer, keeping attributes, and sets the CRS from the
    /// field when the layer has none.
    pub fn transform_layer(&self, layer: &Layer) -> Layer {
        let features = layer
            .features
            .iter()
            .map(|f| Feature {
                geometry: f.geometry.as_ref().map(|g| self.transform(g)),
                values: f.values.clone(),
            })
            .collect();
        Layer {
            name: layer.name.clone(),
            fields: layer.fields.clone(),
            features,
            crs: layer.crs.clone().or_else(|| self.crs.clone()),
        }
    }

    /// Deformed graticule with about `lines` lines along the longer side, spaced equally in
    /// both directions (ScapeToad's "deformation grid" layer).
    pub fn grid_layer(&self, lines: usize) -> Layer {
        let env = self.envelope;
        let res = env.width().max(env.height()) / (lines.max(1) + 1) as f64;
        let nx = (env.width() / res).floor() as usize;
        let ny = (env.height() / res).floor() as usize;
        let mut layer = Layer::new("Deformation grid", vec![Field::new("ID", FieldType::Int)]);
        layer.crs = self.crs.clone();
        let (x0, y0, x1, y1) = (env.min().x, env.min().y, env.max().x, env.max().y);
        let horizontal = (0..=ny).map(|k| {
            let y = y0 + k as f64 * res;
            [Coord { x: x0, y }, Coord { x: x1, y }]
        });
        let vertical = (0..=nx).map(|k| {
            let x = x0 + k as f64 * res;
            [Coord { x, y: y0 }, Coord { x, y: y1 }]
        });
        for (id, ends) in horizontal.chain(vertical).enumerate() {
            layer.features.push(Feature {
                geometry: Some(Geometry::LineString(LineString::new(
                    self.project_coords(&ends),
                ))),
                values: vec![Value::Int(id as i64)],
            });
        }
        layer
    }

    /// Restores constrained cells (flag `1`) towards squares after diffusion.
    ///
    /// `flags` has one entry per cell (`j * (nx - 1) + i`): `-1` empty, `0` deformed by the
    /// master layer, `1` constrained. A node only moves when none of the cells sharing it are
    /// being deformed, so the constrained shapes never fight the cartogram. Port of
    /// `CartogramGrid.conformToConstrainedDeformation`, with its index bugs fixed (the Java
    /// version read the wrong coordinate for one corner and could index out of bounds).
    pub(crate) fn conform_to_constraints(&mut self, flags: &[i8]) {
        let (cnx, cny) = (self.nx - 1, self.ny - 1);
        let flag = |i: isize, j: isize| -> i8 {
            if i < 0 || j < 0 || i >= cnx as isize || j >= cny as isize {
                -1
            } else {
                flags[j as usize * cnx + i as usize]
            }
        };
        // Whether a node shared by cell (i, j) and the three cells at offsets (di, dj) may move.
        let free = |i: isize, j: isize, di: isize, dj: isize| {
            flag(i + di, j) != 0 && flag(i + di, j + dj) != 0 && flag(i, j + dj) != 0
        };

        for j in 0..cny as isize {
            for i in 0..cnx as isize {
                if flag(i, j) != 1 {
                    continue;
                }
                let (ui, uj) = (i as usize, j as usize);
                let ul = self.node(ui, uj + 1);
                let ur = self.node(ui + 1, uj + 1);
                let lr = self.node(ui + 1, uj);
                let ll = self.node(ui, uj);

                // Square with the same area as the current cell, centred on it.
                let (mut min_x, mut max_x) = ((ul.x + ll.x) / 2.0, (ur.x + lr.x) / 2.0);
                let (mut min_y, mut max_y) = ((ll.y + lr.y) / 2.0, (ul.y + ur.y) / 2.0);
                let edge = ((max_x - min_x) * (max_y - min_y)).abs().sqrt();
                let dx = edge - (max_x - min_x);
                let dy = edge - (max_y - min_y);
                min_x -= dx / 2.0;
                max_x += dx / 2.0;
                min_y -= dy / 2.0;
                max_y += dy / 2.0;

                if free(i, j, -1, 1) {
                    self.try_move_node(ui, uj + 1, Coord { x: min_x, y: max_y });
                }
                if free(i, j, 1, 1) {
                    self.try_move_node(ui + 1, uj + 1, Coord { x: max_x, y: max_y });
                }
                if free(i, j, 1, -1) {
                    self.try_move_node(ui + 1, uj, Coord { x: max_x, y: min_y });
                }
                if free(i, j, -1, -1) {
                    self.try_move_node(ui, uj, Coord { x: min_x, y: min_y });
                }
            }
        }
    }

    /// Moves a node towards `target`, by at most a tenth of a cell, and only if it stays
    /// strictly inside its ring of neighbours (no fold-over).
    fn try_move_node(&mut self, i: usize, j: usize, target: Coord<f64>) {
        let cur = self.node(i, j);
        let max_move = self.cell_size().0 / 10.0;
        let (vx, vy) = (target.x - cur.x, target.y - cur.y);
        let dist = vx.hypot(vy);
        let to = if dist > max_move {
            let f = max_move / dist;
            Coord {
                x: cur.x + f * vx,
                y: cur.y + f * vy,
            }
        } else {
            target
        };

        let (i, j) = (i as isize, j as isize);
        let in_grid =
            |a: isize, b: isize| a >= 0 && b >= 0 && a < self.nx as isize && b < self.ny as isize;
        let node = |a: isize, b: isize| self.node(a as usize, b as usize);
        for d in -1..=1 {
            if in_grid(i - 1, j + d) && node(i - 1, j + d).x >= to.x {
                return;
            }
            if in_grid(i + 1, j + d) && node(i + 1, j + d).x <= to.x {
                return;
            }
            if in_grid(i + d, j - 1) && node(i + d, j - 1).y >= to.y {
                return;
            }
            if in_grid(i + d, j + 1) && node(i + d, j + 1).y <= to.y {
                return;
            }
        }
        self.set_node(i as usize, j as usize, to);
    }

    /// Writes the field in a small little-endian binary format (see [`Self::read_from`]).
    pub fn write_to(&self, mut w: impl Write) -> Result<()> {
        w.write_all(MAGIC)?;
        w.write_all(&FORMAT_VERSION.to_le_bytes())?;
        w.write_all(&(self.nx as u32).to_le_bytes())?;
        w.write_all(&(self.ny as u32).to_le_bytes())?;
        for v in [
            self.envelope.min().x,
            self.envelope.min().y,
            self.envelope.max().x,
            self.envelope.max().y,
        ] {
            w.write_all(&v.to_le_bytes())?;
        }
        let epsg = self.crs.as_ref().and_then(|c| c.epsg).unwrap_or(0);
        let wkt = self
            .crs
            .as_ref()
            .and_then(|c| c.wkt.as_deref())
            .unwrap_or("");
        w.write_all(&epsg.to_le_bytes())?;
        w.write_all(&(wkt.len() as u32).to_le_bytes())?;
        w.write_all(wkt.as_bytes())?;
        for v in self.xs.iter().chain(&self.ys) {
            w.write_all(&v.to_le_bytes())?;
        }
        w.flush()?;
        Ok(())
    }

    /// Reads a field written by [`Self::write_to`].
    ///
    /// Layout: magic `STDFIELD`, u32 version, u32 nx, u32 ny, 4×f64 envelope
    /// (min x, min y, max x, max y), i32 EPSG (0 = none), u32 WKT length + UTF-8 WKT,
    /// then nx·ny f64 x coordinates and nx·ny f64 y coordinates, row by row from the bottom.
    ///
    /// Malformed input (truncated, inconsistent sizes, non-finite values) gives
    /// [`Error::InvalidField`]; sizes are checked against the data actually present before
    /// anything large is allocated.
    pub fn read_from(r: impl Read) -> Result<Self> {
        Self::read_unchecked(r).map_err(|e| match e {
            Error::Io(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => truncated(),
            e => e,
        })
    }

    fn read_unchecked(mut r: impl Read) -> Result<Self> {
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(Error::InvalidField("not a deformation field file".into()));
        }
        let version = read_u32(&mut r)?;
        if version != FORMAT_VERSION {
            return Err(Error::InvalidField(format!(
                "unsupported version {version}"
            )));
        }
        let nx = read_u32(&mut r)? as usize;
        let ny = read_u32(&mut r)? as usize;
        if nx < 3 || ny < 3 {
            return Err(Error::GridTooSmall(nx, ny));
        }
        let envelope = Rect::new(
            (read_f64(&mut r)?, read_f64(&mut r)?),
            (read_f64(&mut r)?, read_f64(&mut r)?),
        );
        check_envelope(&envelope)?;
        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        let epsg = i32::from_le_bytes(buf4);
        let wkt_len = read_u32(&mut r)? as usize;
        let wkt = read_bytes(&mut r, wkt_len)?;
        let wkt = String::from_utf8(wkt).map_err(|e| Error::InvalidField(e.to_string()))?;
        let crs = (epsg != 0 || !wkt.is_empty()).then(|| Crs {
            epsg: (epsg != 0).then_some(epsg),
            wkt: (!wkt.is_empty()).then_some(wkt),
        });
        let (n, len) = nx
            .checked_mul(ny)
            .and_then(|n| Some((n, n.checked_mul(16)?)))
            .ok_or_else(|| Error::InvalidField(format!("a {nx}x{ny} grid is too large")))?;
        let bytes = read_bytes(&mut r, len)?;
        let mut vals = bytes
            .chunks_exact(8)
            .map(|b| f64::from_le_bytes(b.try_into().expect("chunk of 8")));
        let xs: Vec<f64> = vals.by_ref().take(n).collect();
        let ys: Vec<f64> = vals.collect();
        if !xs.iter().chain(&ys).all(|v| v.is_finite()) {
            return Err(Error::InvalidField("non-finite node coordinates".into()));
        }
        Ok(Self {
            envelope,
            nx,
            ny,
            xs,
            ys,
            crs,
        })
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.write_to(BufWriter::new(File::create(path)?))
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        Self::read_from(BufReader::new(File::open(path)?))
    }
}

/// Appends the parameters `t` in (0, 1) where the segment `a -> b` (in grid units along one
/// axis) crosses an interior grid line of a grid with `n` nodes.
fn crossings(a: f64, b: f64, n: usize, ts: &mut Vec<f64>) {
    if a == b || !a.is_finite() || !b.is_finite() {
        return;
    }
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let first = (lo.floor() as i64 + 1).max(1);
    let last = (hi.ceil() as i64 - 1).min(n as i64 - 2);
    for k in first..=last {
        let t = (k as f64 - a) / (b - a);
        if t > 0.0 && t < 1.0 {
            ts.push(t);
        }
    }
}

/// Rejects an envelope that cannot carry a grid: non-finite, or without width or height.
fn check_envelope(env: &Rect<f64>) -> Result<()> {
    let (min, max) = (env.min(), env.max());
    let finite = [min.x, min.y, max.x, max.y].iter().all(|v| v.is_finite());
    if finite && max.x > min.x && max.y > min.y {
        Ok(())
    } else {
        Err(Error::InvalidField(format!(
            "the envelope ({}, {}, {}, {}) must be finite, with a positive width and height",
            min.x, min.y, max.x, max.y
        )))
    }
}

fn truncated() -> Error {
    Error::InvalidField("the file ends early".into())
}

/// Reads exactly `len` bytes. The buffer grows with the data actually read, so a corrupt
/// length in a short file is an error rather than a huge allocation.
fn read_bytes(r: &mut impl Read, len: usize) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    r.take(len as u64).read_to_end(&mut buf)?;
    if buf.len() < len {
        return Err(truncated());
    }
    Ok(buf)
}

fn read_u32(r: &mut impl Read) -> Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_f64(r: &mut impl Read) -> Result<f64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(f64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Validation;

    fn field() -> DeformationField {
        DeformationField::identity(Rect::new((0.0, 0.0), (10.0, 5.0)), 11, 6).unwrap()
    }

    #[test]
    fn identity_projects_points_onto_themselves() {
        let f = field();
        for c in [
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 3.3, y: 4.1 },
            Coord { x: 10.0, y: 5.0 },
            Coord { x: 7.25, y: 0.5 },
        ] {
            let p = f.project_coord(c);
            assert!(
                (p.x - c.x).abs() < 1e-12 && (p.y - c.y).abs() < 1e-12,
                "{c:?} -> {p:?}"
            );
        }
    }

    #[test]
    fn segments_get_a_vertex_at_each_grid_and_diagonal_crossing() {
        let f = field();
        // From x=0.5 to x=3.5 at y=2.5 crosses the lines x=1, 2, 3 and, mid-cell, the
        // diagonals of cells 1 and 2 (it starts exactly on the diagonal of cell 0).
        let out = f.project_coords(&[Coord { x: 0.5, y: 2.5 }, Coord { x: 3.5, y: 2.5 }]);
        let xs: Vec<f64> = out.iter().map(|c| c.x).collect();
        assert_eq!(xs, vec![0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.5]);
    }

    fn collinear(p: Coord<f64>, q: Coord<f64>, r: Coord<f64>) -> bool {
        let cross = (q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x);
        cross.abs() < 1e-12
    }

    #[test]
    fn straight_segments_stay_straight_within_a_triangle() {
        // With a deformed node, bilinear interpolation would bend this segment; on the lower
        // triangle of cell (3, 2) the map is affine.
        let mut f = field();
        f.set_node(4, 3, Coord { x: 4.6, y: 3.3 });
        let pts =
            [(3.2, 2.1), (3.5, 2.25), (3.8, 2.4)].map(|(x, y)| f.project_coord(Coord { x, y }));
        assert!(collinear(pts[0], pts[1], pts[2]), "{pts:?}");
    }

    #[test]
    fn non_convex_cell_is_split_along_its_valid_diagonal() {
        // Pull corner b of cell (3, 2) inside the cell: a–c would fold, b–d does not.
        let mut f = field();
        f.set_node(4, 2, Coord { x: 3.3, y: 2.6 });
        assert!(!f.splits_along_ac(3, 2));
        // The cell maps one-to-one: a closed loop around its interior keeps its orientation.
        let ring: Vec<Coord<f64>> = [(3.1, 2.1), (3.9, 2.1), (3.9, 2.9), (3.1, 2.9), (3.1, 2.1)]
            .map(|(x, y)| Coord { x, y })
            .to_vec();
        let out = LineString::new(f.project_coords(&ring));
        assert!(Polygon::new(out, vec![]).is_valid());
    }

    #[test]
    fn near_duplicate_vertices_are_dropped() {
        let f = field();
        let ring = [
            Coord { x: 1.2, y: 1.2 },
            Coord { x: 1.7, y: 1.2 },
            Coord {
                x: 1.7 + 1e-12,
                y: 1.2,
            },
            Coord { x: 1.7, y: 1.7 },
            Coord {
                x: 1.2,
                y: 1.2 + 1e-12,
            },
            Coord { x: 1.2, y: 1.2 },
        ];
        let out = f.project_coords(&ring);
        // The first duplicate goes; the last vertex replaces the one just before it.
        assert_eq!(out.len(), 4);
        assert_eq!(out.first(), out.last());
    }

    #[test]
    fn closed_rings_stay_closed() {
        let f = field();
        let ring = [
            Coord { x: 1.2, y: 1.1 },
            Coord { x: 6.7, y: 1.3 },
            Coord { x: 4.0, y: 4.4 },
            Coord { x: 1.2, y: 1.1 },
        ];
        let out = f.project_coords(&ring);
        assert_eq!(out.first(), out.last());
    }

    #[test]
    fn binary_round_trip() {
        let mut f = field();
        f.set_node(3, 2, Coord { x: 3.2, y: 2.1 });
        f.crs = Some(Crs {
            epsg: Some(21781),
            wkt: Some("PROJCS[\"CH1903 / LV03\"]".into()),
        });
        let mut buf = Vec::new();
        f.write_to(&mut buf).unwrap();
        assert_eq!(DeformationField::read_from(buf.as_slice()).unwrap(), f);
    }

    /// File header up to the WKT, which is left out like the node coordinates.
    fn header(nx: u32, ny: u32, envelope: [f64; 4], wkt_len: u32) -> Vec<u8> {
        let mut h = MAGIC.to_vec();
        for v in [FORMAT_VERSION, nx, ny] {
            h.extend(v.to_le_bytes());
        }
        for v in envelope {
            h.extend(v.to_le_bytes());
        }
        h.extend(0i32.to_le_bytes());
        h.extend(wkt_len.to_le_bytes());
        h
    }

    #[test]
    fn identity_needs_a_finite_envelope() {
        for (x1, y1) in [(f64::INFINITY, 1.0), (f64::NAN, 1.0), (0.0, 1.0)] {
            let res = DeformationField::identity(Rect::new((0.0, 0.0), (x1, y1)), 3, 3);
            assert!(matches!(res, Err(Error::InvalidField(_))), "{x1} {y1}");
        }
    }

    #[test]
    fn malformed_files_are_errors() {
        let unit = [0.0, 0.0, 1.0, 1.0];
        let mut valid = Vec::new();
        field().write_to(&mut valid).unwrap();
        let mut nan_node = valid.clone();
        let last = valid.len() - 8;
        nan_node[last..].copy_from_slice(&f64::NAN.to_le_bytes());
        let cases = [
            ("truncated header", valid[..30].to_vec()),
            ("truncated nodes", valid[..valid.len() - 1].to_vec()),
            ("grid size overflowing", header(u32::MAX, u32::MAX, unit, 0)),
            ("large grid, no data", header(1 << 20, 1 << 20, unit, 0)),
            ("large WKT, no data", header(3, 3, unit, u32::MAX)),
            ("empty envelope", header(3, 3, [1.0, 0.0, 1.0, 1.0], 0)),
            ("NaN envelope", header(3, 3, [f64::NAN, 0.0, 1.0, 1.0], 0)),
            ("NaN node", nan_node),
        ];
        for (what, bytes) in cases {
            let res = DeformationField::read_from(bytes.as_slice());
            assert!(
                matches!(res, Err(Error::InvalidField(_))),
                "{what}: {res:?}"
            );
        }
    }
}
