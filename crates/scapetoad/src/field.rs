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
    /// Undeformed field: every node at its original position.
    pub fn identity(envelope: Rect<f64>, nx: usize, ny: usize) -> Result<Self> {
        if nx < 3 || ny < 3 {
            return Err(Error::GridTooSmall(nx, ny));
        }
        let mut field = Self {
            envelope,
            nx,
            ny,
            xs: vec![0.0; nx * ny],
            ys: vec![0.0; nx * ny],
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

    /// Projects one coordinate by bilinear interpolation in the deformed cell containing it.
    ///
    /// Points outside the envelope are extrapolated from the nearest border cell. ScapeToad
    /// scaled by `n / width` instead of `(n - 1) / width` here, which sampled the wrong cell
    /// by up to one cell width; this uses the node spacing consistently.
    pub fn project_coord(&self, c: Coord<f64>) -> Coord<f64> {
        let (gx, gy) = self.to_grid(c);
        self.project_grid(gx, gy)
    }

    fn project_grid(&self, gx: f64, gy: f64) -> Coord<f64> {
        let i = (gx.floor() as isize).clamp(0, self.nx as isize - 2) as usize;
        let j = (gy.floor() as isize).clamp(0, self.ny as isize - 2) as usize;
        let ti = gx - i as f64;
        let tj = gy - j as f64;
        let a = self.node(i, j);
        let b = self.node(i + 1, j);
        let c = self.node(i + 1, j + 1);
        let d = self.node(i, j + 1);
        let (wa, wb, wc, wd) = (
            (1.0 - ti) * (1.0 - tj),
            ti * (1.0 - tj),
            ti * tj,
            (1.0 - ti) * tj,
        );
        Coord {
            x: wa * a.x + wb * b.x + wc * c.x + wd * d.x,
            y: wa * a.y + wb * b.y + wc * c.y + wd * d.y,
        }
    }

    /// Projects a coordinate sequence, inserting a vertex wherever a segment crosses a grid
    /// line. Inside a cell the bilinear map bends straight lines, so without these extra
    /// vertices long segments would cut across neighbouring cells and could self-intersect.
    pub fn project_coords(&self, coords: &[Coord<f64>]) -> Vec<Coord<f64>> {
        let mut out = Vec::with_capacity(coords.len() * 2);
        let Some(&first) = coords.first() else {
            return out;
        };
        out.push(self.project_coord(first));
        let mut ts: Vec<f64> = Vec::new();
        for w in coords.windows(2) {
            let (p, q) = (self.to_grid(w[0]), self.to_grid(w[1]));
            ts.clear();
            crossings(p.0, q.0, self.nx, &mut ts);
            crossings(p.1, q.1, self.ny, &mut ts);
            ts.sort_unstable_by(f64::total_cmp);
            ts.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
            for &t in &ts {
                out.push(self.project_grid(p.0 + t * (q.0 - p.0), p.1 + t * (q.1 - p.1)));
            }
            out.push(self.project_grid(q.0, q.1));
        }
        out
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
    pub fn read_from(mut r: impl Read) -> Result<Self> {
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
        let (x0, y0, x1, y1) = (
            read_f64(&mut r)?,
            read_f64(&mut r)?,
            read_f64(&mut r)?,
            read_f64(&mut r)?,
        );
        let mut buf4 = [0u8; 4];
        r.read_exact(&mut buf4)?;
        let epsg = i32::from_le_bytes(buf4);
        let wkt_len = read_u32(&mut r)? as usize;
        let mut wkt = vec![0u8; wkt_len];
        r.read_exact(&mut wkt)?;
        let wkt = String::from_utf8(wkt).map_err(|e| Error::InvalidField(e.to_string()))?;
        let crs = (epsg != 0 || !wkt.is_empty()).then(|| Crs {
            epsg: (epsg != 0).then_some(epsg),
            wkt: (!wkt.is_empty()).then_some(wkt),
        });
        let n = nx * ny;
        let mut bytes = vec![0u8; 2 * n * 8];
        r.read_exact(&mut bytes)?;
        let mut vals = bytes
            .chunks_exact(8)
            .map(|b| f64::from_le_bytes(b.try_into().expect("chunk of 8")));
        let xs: Vec<f64> = vals.by_ref().take(n).collect();
        let ys: Vec<f64> = vals.collect();
        Ok(Self {
            envelope: Rect::new((x0, y0), (x1, y1)),
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
    fn segments_get_a_vertex_at_each_grid_crossing() {
        let f = field();
        // From x=0.5 to x=3.5 at constant y crosses the lines x=1, 2, 3.
        let out = f.project_coords(&[Coord { x: 0.5, y: 2.5 }, Coord { x: 3.5, y: 2.5 }]);
        let xs: Vec<f64> = out.iter().map(|c| c.x).collect();
        assert_eq!(xs, vec![0.5, 1.0, 2.0, 3.0, 3.5]);
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
}
