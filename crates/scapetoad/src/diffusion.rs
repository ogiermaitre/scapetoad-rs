//! Gastner–Newman diffusion cartogram (PNAS 2004), ported from ScapeToad's `CartogramNewman`,
//! itself a Java adaptation of Mark Newman's `cart` C code.
//!
//! The density is diffused with the heat equation, solved in the cosine basis so the
//! boundary is reflecting. Grid points are advected with the velocity field
//! `v = -∇ρ / ρ` by adaptive fourth-order Runge–Kutta until they stop moving.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use rustdct::{DctPlanner, TransformType2And3};

/// Initial size of a time step.
const INIT_H: f64 = 0.001;
/// Desired accuracy per step, in grid cells.
const TARGET_ERROR: f64 = 0.01;
/// Maximum factor by which the step size may grow between iterations.
const MAX_RATIO: f64 = 4.0;
/// Rough guess of the final time, only used to estimate progress.
const EXPECTED_TIME: f64 = 1.0e8;

/// Stopping criteria for the diffusion.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffusionOptions {
    /// Stop once no grid point moved more than this (in grid cells) during an iteration.
    /// ScapeToad waited for exactly zero; a tiny threshold gives the same map without relying
    /// on the transform producing a perfectly flat density at the end.
    pub tolerance: f64,
    /// Hard cap on the number of (double) integration steps.
    pub max_iterations: usize,
    /// Stop after this wall-clock time, keeping the current state.
    pub max_duration: Option<Duration>,
}

impl Default for DiffusionOptions {
    fn default() -> Self {
        Self {
            tolerance: 1e-7,
            max_iterations: 10_000,
            max_duration: Some(Duration::from_secs(3 * 3600)),
        }
    }
}

/// State reported after each integration step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiffusionStep {
    pub iteration: usize,
    /// Diffusion time reached.
    pub time: f64,
    /// Largest displacement of any point during the step, in grid cells.
    pub max_displacement: f64,
    /// Rough completion estimate in [0, 1], based on the time reached.
    pub fraction: f64,
}

/// Why the diffusion stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Converged,
    MaxIterations,
    Timeout,
}

/// Final positions of the `(lx + 1) * (ly + 1)` grid points, in grid units.
pub(crate) struct DiffusionResult {
    pub xs: Vec<f64>,
    pub ys: Vec<f64>,
    pub iterations: usize,
    pub stop: StopReason,
}

/// Separable 2D DCT on a row-major `ly × lx` array (index `iy * lx + ix`).
///
/// Uses unnormalised DCT-II forward and DCT-III inverse: the round trip scales the data by a
/// constant, which is harmless because the velocity `∇ρ / ρ` does not depend on the scale.
struct Dct2d {
    lx: usize,
    ly: usize,
    plan_x: Arc<dyn TransformType2And3<f64>>,
    plan_y: Arc<dyn TransformType2And3<f64>>,
}

impl Dct2d {
    fn new(lx: usize, ly: usize) -> Self {
        let mut planner = DctPlanner::new();
        Self {
            lx,
            ly,
            plan_x: planner.plan_dct2(lx),
            plan_y: planner.plan_dct2(ly),
        }
    }

    fn forward(&self, data: &mut [f64], tmp: &mut [f64]) {
        self.apply(data, tmp, false);
    }

    fn inverse(&self, data: &mut [f64], tmp: &mut [f64]) {
        self.apply(data, tmp, true);
    }

    /// Transforms rows, then columns through a transposed copy so both passes are contiguous.
    fn apply(&self, data: &mut [f64], tmp: &mut [f64], inverse: bool) {
        let (lx, ly) = (self.lx, self.ly);
        run_rows(&*self.plan_x, data, lx, inverse);
        tmp.par_chunks_mut(ly).enumerate().for_each(|(ix, col)| {
            for (iy, v) in col.iter_mut().enumerate() {
                *v = data[iy * lx + ix];
            }
        });
        run_rows(&*self.plan_y, tmp, ly, inverse);
        data.par_chunks_mut(lx).enumerate().for_each(|(iy, row)| {
            for (ix, v) in row.iter_mut().enumerate() {
                *v = tmp[ix * ly + iy];
            }
        });
    }
}

fn run_rows(plan: &dyn TransformType2And3<f64>, data: &mut [f64], len: usize, inverse: bool) {
    let scratch_len = plan.get_scratch_len();
    data.par_chunks_mut(len).for_each_init(
        || vec![0.0; scratch_len],
        |scratch, row| {
            if inverse {
                plan.process_dct3_with_scratch(row, scratch);
            } else {
                plan.process_dct2_with_scratch(row, scratch);
            }
        },
    );
}

/// Diffusion of a density grid of `lx × ly` cells.
struct Diffusion {
    lx: usize,
    ly: usize,
    dct: Dct2d,
    /// Spectrum of the initial density.
    spectrum: Vec<f64>,
    /// Density snapshots at the five time slices used by one double RK4 step.
    snapshots: [Vec<f64>; 5],
    tmp: Vec<f64>,
}

impl Diffusion {
    fn new(rho: &[f64], lx: usize, ly: usize) -> Self {
        let dct = Dct2d::new(lx, ly);
        let mut spectrum = rho.to_vec();
        let mut tmp = vec![0.0; lx * ly];
        dct.forward(&mut spectrum, &mut tmp);
        Self {
            lx,
            ly,
            dct,
            spectrum,
            snapshots: std::array::from_fn(|_| vec![0.0; lx * ly]),
            tmp,
        }
    }

    /// Density at time `t`, written into snapshot `s`: each cosine mode decays as
    /// `exp(-(kx² + ky²) t)`.
    fn snapshot(&mut self, t: f64, s: usize) {
        let (lx, ly) = (self.lx, self.ly);
        let expkx: Vec<f64> = (0..lx)
            .map(|ix| {
                let k = std::f64::consts::PI * ix as f64 / lx as f64;
                (-k * k * t).exp()
            })
            .collect();
        let expky: Vec<f64> = (0..ly)
            .map(|iy| {
                let k = std::f64::consts::PI * iy as f64 / ly as f64;
                (-k * k * t).exp()
            })
            .collect();
        let spectrum = &self.spectrum;
        let out = &mut self.snapshots[s];
        out.par_chunks_mut(lx).enumerate().for_each(|(iy, row)| {
            for (ix, v) in row.iter_mut().enumerate() {
                *v = spectrum[iy * lx + ix] * expkx[ix] * expky[iy];
            }
        });
        self.dct.inverse(out, &mut self.tmp);
    }

    /// Upper bound, in grid cells, on how far any point can still move after time `t`.
    ///
    /// The density is `Σ A_k cos(k·r) e^{-|k|²t}`, so `|∇ρ| ≤ Σ |A_k| |k| e^{-|k|²t}` and
    /// `ρ ≥ A_0 - Σ |A_k| e^{-|k|²t}`. Integrating `|∇ρ| / ρ` from `t` to infinity gives
    /// `Σ |A_k| e^{-|k|²t} / |k|` over that lower bound on `ρ`.
    ///
    /// Without this test, the run cannot stop once the density is flat: the transform's
    /// rounding noise leaves a velocity of about 1e-14, the step keeps growing fourfold, and
    /// the points end up drifting by a cell per step.
    fn remaining_motion(&self, t: f64) -> f64 {
        let (lx, ly) = (self.lx, self.ly);
        let c00 = self.spectrum[0];
        if c00 <= 0.0 {
            return f64::INFINITY;
        }
        let kx = |ix: usize| std::f64::consts::PI * ix as f64 / lx as f64;
        let ky = |iy: usize| std::f64::consts::PI * iy as f64 / ly as f64;
        let (motion, amplitude) = self
            .spectrum
            .par_chunks(lx)
            .enumerate()
            .map(|(iy, row)| {
                let mut acc = (0.0, 0.0);
                for (ix, &c) in row.iter().enumerate() {
                    if ix == 0 && iy == 0 {
                        continue;
                    }
                    let k2 = kx(ix).powi(2) + ky(iy).powi(2);
                    // The inverse transform weighs non-zero frequencies twice per axis.
                    let weight = if ix > 0 { 2.0 } else { 1.0 } * if iy > 0 { 2.0 } else { 1.0 };
                    let a = weight * c.abs() / c00 * (-k2 * t).exp();
                    acc.0 += a / k2.sqrt();
                    acc.1 += a;
                }
                acc
            })
            .reduce(|| (0.0, 0.0), |a, b| (a.0 + b.0, a.1 + b.1));
        if amplitude >= 1.0 {
            f64::INFINITY
        } else {
            motion / (1.0 - amplitude)
        }
    }

    /// Integrates `2h` ahead from time `t` with one big RK4 step and two small ones, moves the
    /// points using the small steps plus local extrapolation, and returns the maximum error
    /// and displacement (both in grid cells) and the snapshot index for time `t + 2h`.
    fn integrate_two_steps(
        &mut self,
        t: f64,
        h: f64,
        s: usize,
        xs: &mut [f64],
        ys: &mut [f64],
    ) -> (f64, f64, usize) {
        let s0 = s;
        let [s1, s2, s3, s4] = [1, 2, 3, 4].map(|d| (s + d) % 5);
        self.snapshot(t + 0.5 * h, s1);
        self.snapshot(t + h, s2);
        self.snapshot(t + 1.5 * h, s3);
        self.snapshot(t + 2.0 * h, s4);

        let field = VelocityField {
            lx: self.lx,
            ly: self.ly,
        };
        let snaps = &self.snapshots;
        let v = |x: f64, y: f64, s: usize| field.velocity(&snaps[s], x, y);
        let (lx, ly) = (self.lx as f64, self.ly as f64);

        let (esq_max, drsq_max) = xs
            .par_iter_mut()
            .zip(ys.par_iter_mut())
            .map(|(px, py)| {
                let (rx1, ry1) = (*px, *py);

                // One big 2h step.
                let v1 = v(rx1, ry1, s0);
                let k1 = (2.0 * h * v1.0, 2.0 * h * v1.1);
                let v2 = v(rx1 + 0.5 * k1.0, ry1 + 0.5 * k1.1, s2);
                let k2 = (2.0 * h * v2.0, 2.0 * h * v2.1);
                let v3 = v(rx1 + 0.5 * k2.0, ry1 + 0.5 * k2.1, s2);
                let k3 = (2.0 * h * v3.0, 2.0 * h * v3.1);
                let v4 = v(rx1 + k3.0, ry1 + k3.1, s4);
                let k4 = (2.0 * h * v4.0, 2.0 * h * v4.1);
                let d12 = rk4_increment(k1, k2, k3, k4);

                // First small step (reuses v1, which is the same evaluation).
                let k1 = (h * v1.0, h * v1.1);
                let v2 = v(rx1 + 0.5 * k1.0, ry1 + 0.5 * k1.1, s1);
                let k2 = (h * v2.0, h * v2.1);
                let v3 = v(rx1 + 0.5 * k2.0, ry1 + 0.5 * k2.1, s1);
                let k3 = (h * v3.0, h * v3.1);
                let v4 = v(rx1 + k3.0, ry1 + k3.1, s2);
                let k4 = (h * v4.0, h * v4.1);
                let d1 = rk4_increment(k1, k2, k3, k4);

                // Second small step.
                let (rx2, ry2) = (rx1 + d1.0, ry1 + d1.1);
                let v1 = v(rx2, ry2, s2);
                let k1 = (h * v1.0, h * v1.1);
                let v2 = v(rx2 + 0.5 * k1.0, ry2 + 0.5 * k1.1, s3);
                let k2 = (h * v2.0, h * v2.1);
                let v3 = v(rx2 + 0.5 * k2.0, ry2 + 0.5 * k2.1, s3);
                let k3 = (h * v3.0, h * v3.1);
                let v4 = v(rx2 + k3.0, ry2 + k3.1, s4);
                let k4 = (h * v4.0, h * v4.1);
                let d2 = rk4_increment(k1, k2, k3, k4);

                // Richardson error estimate, also added back as 5th-order local extrapolation.
                let ex = (d1.0 + d2.0 - d12.0) / 15.0;
                let ey = (d1.1 + d2.1 - d12.1) / 15.0;
                let dx = d1.0 + d2.0 + ex;
                let dy = d1.1 + d2.1 + ey;

                *px = (rx1 + dx).clamp(0.0, lx);
                *py = (ry1 + dy).clamp(0.0, ly);
                (ex * ex + ey * ey, dx * dx + dy * dy)
            })
            .reduce(|| (0.0, 0.0), |a, b| (a.0.max(b.0), a.1.max(b.1)));

        (esq_max.sqrt(), drsq_max.sqrt(), s4)
    }
}

fn rk4_increment(k1: (f64, f64), k2: (f64, f64), k3: (f64, f64), k4: (f64, f64)) -> (f64, f64) {
    (
        (k1.0 + k4.0 + 2.0 * (k2.0 + k3.0)) / 6.0,
        (k1.1 + k4.1 + 2.0 * (k2.1 + k3.1)) / 6.0,
    )
}

#[derive(Clone, Copy)]
struct VelocityField {
    lx: usize,
    ly: usize,
}

impl VelocityField {
    /// Velocity at `(rx, ry)` (grid units): `-∇ρ/ρ` at the four surrounding grid points from
    /// central differences over the 3×3 neighbouring cells, then bilinear interpolation.
    /// Indices are clamped at the border, which makes the normal velocity vanish there.
    #[inline]
    fn velocity(&self, rho: &[f64], rx: f64, ry: f64) -> (f64, f64) {
        let (lx, ly) = (self.lx, self.ly);
        let ix = (rx as isize).clamp(0, lx as isize - 1) as usize;
        let iy = (ry as isize).clamp(0, ly as isize - 1) as usize;
        let ixm1 = ix.saturating_sub(1);
        let ixp1 = (ix + 1).min(lx - 1);
        let iym1 = iy.saturating_sub(1);
        let iyp1 = (iy + 1).min(ly - 1);
        let r = |x: usize, y: usize| rho[y * lx + x];

        let rho00 = r(ixm1, iym1);
        let rho10 = r(ix, iym1);
        let rho20 = r(ixp1, iym1);
        let rho01 = r(ixm1, iy);
        let rho11 = r(ix, iy);
        let rho21 = r(ixp1, iy);
        let rho02 = r(ixm1, iyp1);
        let rho12 = r(ix, iyp1);
        let rho22 = r(ixp1, iyp1);

        let mid11 = rho00 + rho10 + rho01 + rho11;
        let vx11 = -2.0 * (rho10 - rho00 + rho11 - rho01) / mid11;
        let vy11 = -2.0 * (rho01 - rho00 + rho11 - rho10) / mid11;

        let mid21 = rho10 + rho20 + rho11 + rho21;
        let vx21 = -2.0 * (rho20 - rho10 + rho21 - rho11) / mid21;
        let vy21 = -2.0 * (rho11 - rho10 + rho21 - rho20) / mid21;

        let mid12 = rho01 + rho11 + rho02 + rho12;
        let vx12 = -2.0 * (rho11 - rho01 + rho12 - rho02) / mid12;
        let vy12 = -2.0 * (rho02 - rho01 + rho12 - rho11) / mid12;

        let mid22 = rho11 + rho21 + rho12 + rho22;
        let vx22 = -2.0 * (rho21 - rho11 + rho22 - rho12) / mid22;
        let vy22 = -2.0 * (rho12 - rho11 + rho22 - rho21) / mid22;

        let dx = rx - ix as f64;
        let dy = ry - iy as f64;
        let w11 = (1.0 - dx) * (1.0 - dy);
        let w21 = dx * (1.0 - dy);
        let w12 = (1.0 - dx) * dy;
        let w22 = dx * dy;

        (
            w11 * vx11 + w21 * vx21 + w12 * vx12 + w22 * vx22,
            w11 * vy11 + w21 * vy21 + w12 * vy12 + w22 * vy22,
        )
    }
}

/// Runs the diffusion on `rho` (`lx × ly` cells, row-major from the bottom) and returns the
/// final positions of the `(lx + 1) × (ly + 1)` cell corners.
pub(crate) fn diffuse(
    rho: &[f64],
    lx: usize,
    ly: usize,
    options: &DiffusionOptions,
    on_step: &mut dyn FnMut(DiffusionStep),
) -> DiffusionResult {
    let start = Instant::now();
    let mut diffusion = Diffusion::new(rho, lx, ly);
    let mut xs = Vec::with_capacity((lx + 1) * (ly + 1));
    let mut ys = Vec::with_capacity((lx + 1) * (ly + 1));
    for iy in 0..=ly {
        for ix in 0..=lx {
            xs.push(ix as f64);
            ys.push(iy as f64);
        }
    }

    let mut s = 0;
    diffusion.snapshot(0.0, s);
    let mut t = 0.0;
    let mut h = INIT_H;
    let mut iteration = 0;
    let stop = loop {
        let (error, max_displacement, next) =
            diffusion.integrate_two_steps(t, h, s, &mut xs, &mut ys);
        t += 2.0 * h;
        s = next;
        iteration += 1;
        // The target for a double step is twice the single-step target.
        let desired = (2.0 * TARGET_ERROR / error).powf(0.2);
        let remaining = diffusion.remaining_motion(t);
        h *= desired.min(MAX_RATIO);

        let fraction = ((t / INIT_H).ln() / (EXPECTED_TIME / INIT_H).ln()).clamp(0.0, 1.0);
        on_step(DiffusionStep {
            iteration,
            time: t,
            max_displacement,
            fraction,
        });

        if max_displacement <= options.tolerance || remaining <= options.tolerance {
            break StopReason::Converged;
        }
        if iteration >= options.max_iterations {
            break StopReason::MaxIterations;
        }
        if options.max_duration.is_some_and(|d| start.elapsed() >= d) {
            break StopReason::Timeout;
        }
    };

    DiffusionResult {
        xs,
        ys,
        iterations: iteration,
        stop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dct_round_trip_is_a_uniform_scaling() {
        let (lx, ly) = (12, 7);
        let data: Vec<f64> = (0..lx * ly).map(|k| ((k * 37) % 11) as f64 + 0.5).collect();
        let dct = Dct2d::new(lx, ly);
        let mut buf = data.clone();
        let mut tmp = vec![0.0; lx * ly];
        dct.forward(&mut buf, &mut tmp);
        dct.inverse(&mut buf, &mut tmp);
        let scale = buf[0] / data[0];
        for (a, b) in buf.iter().zip(&data) {
            assert!((a / b - scale).abs() < 1e-9);
        }
    }

    #[test]
    fn uniform_density_does_not_move_points() {
        let (lx, ly) = (8, 6);
        let res = diffuse(
            &vec![3.0; lx * ly],
            lx,
            ly,
            &DiffusionOptions::default(),
            &mut |_| {},
        );
        assert_eq!(res.stop, StopReason::Converged);
        for (k, (x, y)) in res.xs.iter().zip(&res.ys).enumerate() {
            assert!((x - (k % (lx + 1)) as f64).abs() < 1e-9);
            assert!((y - (k / (lx + 1)) as f64).abs() < 1e-9);
        }
    }

    /// Density 1 with a block of 50 in the lower left quarter.
    fn block_density(lx: usize, ly: usize) -> Vec<f64> {
        let mut rho = vec![1.0; lx * ly];
        for iy in ly / 3..ly / 2 {
            for ix in lx / 3..lx / 2 {
                rho[iy * lx + ix] = 50.0;
            }
        }
        rho
    }

    #[test]
    fn remaining_motion_vanishes_as_the_density_flattens() {
        let (lx, ly) = (40, 30);
        let d = Diffusion::new(&block_density(lx, ly), lx, ly);
        let bounds: Vec<f64> = [1.0, 1e2, 1e4, 1e6].map(|t| d.remaining_motion(t)).to_vec();
        assert!(bounds.windows(2).all(|w| w[1] < w[0]), "{bounds:?}");
        assert!(bounds[3] < 1e-100);
        let flat = Diffusion::new(&vec![2.0; lx * ly], lx, ly);
        assert!(flat.remaining_motion(0.0) < 1e-12);
    }

    #[test]
    fn stops_before_rounding_noise_moves_the_points() {
        // Once the density is flat, the transform's rounding noise is the only velocity left,
        // and the growing time step scales it up: a displacement test alone never ends. The
        // noise shows with lengths the DCT cannot factor (499 is prime), and a tolerance
        // below it makes the failure certain instead of depending on the data.
        let (lx, ly) = (499, 20);
        let options = DiffusionOptions {
            tolerance: 1e-10,
            max_iterations: 100,
            ..DiffusionOptions::default()
        };
        let res = diffuse(&block_density(lx, ly), lx, ly, &options, &mut |_| {});
        assert_eq!(res.stop, StopReason::Converged);
    }

    #[test]
    fn dense_block_expands() {
        // A dense square in the middle must grow: its corners move outwards.
        let (lx, ly) = (32, 32);
        let mut rho = vec![1.0; lx * ly];
        for iy in 12..20 {
            for ix in 12..20 {
                rho[iy * lx + ix] = 4.0;
            }
        }
        let res = diffuse(&rho, lx, ly, &DiffusionOptions::default(), &mut |_| {});
        assert_eq!(res.stop, StopReason::Converged);
        let corner = |ix: usize, iy: usize| {
            let k = iy * (lx + 1) + ix;
            (res.xs[k], res.ys[k])
        };
        let (x0, y0) = corner(12, 12);
        let (x1, y1) = corner(20, 20);
        assert!(x0 < 12.0 && y0 < 12.0 && x1 > 20.0 && y1 > 20.0);
        // The block rounds while it grows, so measure it along its whole boundary (shoelace).
        let boundary: Vec<(f64, f64)> = (12..20)
            .map(|i| corner(i, 12))
            .chain((12..20).map(|j| corner(20, j)))
            .chain((13..=20).rev().map(|i| corner(i, 20)))
            .chain((13..=20).rev().map(|j| corner(12, j)))
            .collect();
        let area = boundary
            .iter()
            .zip(boundary.iter().cycle().skip(1))
            .map(|(a, b)| a.0 * b.1 - b.0 * a.1)
            .sum::<f64>()
            .abs()
            / 2.0;
        // Fair share: mass 256 of 1216 spread over 1024 cells, about 215.6 cells.
        assert!((area - 215.6).abs() / 215.6 < 0.1, "area {area}");
    }
}
