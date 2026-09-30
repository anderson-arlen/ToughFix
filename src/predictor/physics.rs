// SPDX-License-Identifier: MIT
use super::data::{AU, DAY, Frames, GPS_JD, M3, MU, R_EARTH, Spline, V3};
use anyhow::{Result, ensure};
use nalgebra::{DMatrix, DVector};
use ode_solvers::{Dop853, OutputType, System};
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Read, path::Path};

#[derive(Clone)]
pub struct Gravity {
    coeff: [[[f64; 2]; 9]; 9],
}
impl Gravity {
    pub fn read(path: &Path) -> Result<Self> {
        let mut zip = zip::ZipArchive::new(File::open(path)?)?;
        let mut text = String::new();
        zip.by_name("EGM96")?.read_to_string(&mut text)?;
        let mut coeff = [[[0.; 2]; 9]; 9];
        let mut count = 0;
        for line in text.lines() {
            let p: Vec<_> = line.split_whitespace().collect();
            ensure!(p.len() >= 4, "Invalid EGM96 coefficient");
            let n: usize = p[0].parse()?;
            let m: usize = p[1].parse()?;
            if n > 8 {
                break;
            }
            ensure!(m <= n, "Invalid harmonic order");
            let factorial = |k: usize| (1..=k).fold(1., |a, i| a * i as f64);
            let norm = ((if m == 0 { 1. } else { 2. }) * (2 * n + 1) as f64 * factorial(n - m)
                / factorial(n + m))
            .sqrt();
            coeff[n][m] = [p[2].parse::<f64>()? * norm, p[3].parse::<f64>()? * norm];
            count += 1;
        }
        ensure!(count == 42, "Expected all degree 2–8 EGM96 coefficients");
        Ok(Self { coeff })
    }
    pub fn perturbation(&self, r: V3) -> V3 {
        let radius = r.norm();
        let sp = r.z / radius;
        let cp = r.x.hypot(r.y) / radius;
        let lam = r.y.atan2(r.x);
        let mut p = [[0.; 9]; 9];
        let mut dp = [[0.; 9]; 9];
        p[0][0] = 1.;
        let (mut ar, mut ap, mut al) = (0., 0., 0.);
        for n in 1..=8 {
            for m in 0..=n {
                let nf = n as f64;
                let mf = m as f64;
                if n == m {
                    p[n][m] = (2. * nf - 1.) * cp * p[n - 1][m - 1];
                    dp[n][m] = (2. * nf - 1.) * (-sp * p[n - 1][m - 1] + cp * dp[n - 1][m - 1]);
                } else if n == m + 1 {
                    p[n][m] = (2. * nf - 1.) * sp * p[n - 1][m];
                    dp[n][m] = (2. * nf - 1.) * (cp * p[n - 1][m] + sp * dp[n - 1][m]);
                } else {
                    p[n][m] = ((2. * nf - 1.) * sp * p[n - 1][m] - (nf + mf - 1.) * p[n - 2][m])
                        / (nf - mf);
                    dp[n][m] = ((2. * nf - 1.) * (cp * p[n - 1][m] + sp * dp[n - 1][m])
                        - (nf + mf - 1.) * dp[n - 2][m])
                        / (nf - mf);
                }
                if n < 2 {
                    continue;
                }
                let [c, s] = self.coeff[n][m];
                let cm = (mf * lam).cos();
                let sm = (mf * lam).sin();
                let factor = (R_EARTH / radius).powi(n as i32);
                let trig = c * cm + s * sm;
                ar -= (nf + 1.) * factor * p[n][m] * trig;
                ap += factor * dp[n][m] * trig;
                al += factor * p[n][m] * mf * (-c * sm + s * cm) / cp;
            }
        }
        let cl = lam.cos();
        let sl = lam.sin();
        MU / radius.powi(2)
            * V3::new(
                ar * cp * cl - ap * sp * cl - al * sl,
                ar * cp * sl - ap * sp * sl + al * cl,
                ar * sp + ap * cp,
            )
    }
}
pub struct Model {
    pub start: f64,
    gravity: Gravity,
    environment: Spline<15>,
}
impl Model {
    pub fn new(frames: &Frames, gravity: Gravity, start: f64, end: f64) -> Result<Self> {
        let mut values = Vec::new();
        let mut t = start - 600.;
        while t < end + 1200. {
            let ((earth, _), warning) = erfars::ephemerides::Epv00(GPS_JD, (t + 51.184) / DAY)
                .map_err(|e| anyhow::anyhow!("ERFA Earth ephemeris {e:?}"))?;
            ensure!(
                warning == 0,
                "Earth ephemeris is outside its accuracy interval"
            );
            let moon = erfars::ephemerides::Moon98(GPS_JD, (t + 51.184) / DAY);
            let rotation = frames.matrix(t)?;
            let mut v = [0.; 15];
            for k in 0..3 {
                v[k] = -earth[k] * AU;
                v[k + 3] = moon[k] * AU;
            }
            for row in 0..3 {
                for col in 0..3 {
                    v[6 + row * 3 + col] = rotation[(row, col)];
                }
            }
            values.push(v);
            t += 600.;
        }
        Ok(Self {
            start,
            gravity,
            environment: Spline::new(-600., 600., &values)?,
        })
    }
    pub fn acceleration(&self, t: f64, r: V3, v: V3, coeff: [f64; 5]) -> V3 {
        self.acceleration_with(self.environment.at(t), r, v, coeff)
    }
    fn acceleration_with(&self, env: [f64; 15], r: V3, v: V3, coeff: [f64; 5]) -> V3 {
        let radius = r.norm();
        let rotation = M3::from_row_slice(&env[6..]);
        let sun = V3::from_row_slice(&env[..3]);
        let moon = V3::from_row_slice(&env[3..6]);
        let mut a = -MU * r / radius.powi(3)
            + rotation.transpose() * self.gravity.perturbation(rotation * r);
        for (body, mu) in [(sun, 1.32712440018e20), (moon, 4.902800066e12)] {
            let delta = body - r;
            a += mu * (delta / delta.norm().powi(3) - body / body.norm().powi(3));
        }
        let radial = r / radius;
        let d = (sun - r).normalize();
        let y = d.cross(&radial).normalize();
        let b = d.cross(&y);
        let normal = r.cross(&v).normalize();
        let projected = (d - d.dot(&normal) * normal).normalize();
        let cosu = radial.dot(&projected);
        let sinu = radial.dot(&normal.cross(&projected));
        let srp = coeff[0] * d + coeff[1] * y + (coeff[2] + coeff[3] * cosu + coeff[4] * sinu) * b;
        let earth_angle = (R_EARTH / radius).asin();
        let sun_angle = (695700000. / (sun - r).norm()).asin();
        let separation = (-radial.dot(&d)).clamp(-1., 1.).acos();
        let lit = ((separation - earth_angle + sun_angle) / (2. * sun_angle)).clamp(0., 1.);
        a += srp * lit * (AU / (sun - r).norm()).powi(2) * 1e-7;
        a
    }
    pub fn propagate(
        &self,
        initial: &[[f64; 6]],
        forces: &[[f64; 5]],
        end: f64,
        step: f64,
    ) -> Result<Vec<Vec<[f64; 6]>>> {
        self.propagate_with_step(initial, forces, end, step, 60.)
    }
    pub fn propagate_with_step(
        &self,
        initial: &[[f64; 6]],
        forces: &[[f64; 5]],
        end: f64,
        step: f64,
        max_step: f64,
    ) -> Result<Vec<Vec<[f64; 6]>>> {
        ensure!(
            initial.len() == forces.len() && !initial.is_empty() && end > 0. && step > 0.,
            "Invalid propagation inputs"
        );
        let system = OrbitSystem {
            model: self,
            forces,
        };
        let state = DVector::from_iterator(initial.len() * 6, initial.iter().flatten().copied());
        let mut solver = Dop853::from_param(
            system,
            0.,
            end,
            step,
            state,
            2e-13,
            1e-10,
            0.9,
            0.,
            0.333,
            6.,
            max_step,
            0.,
            250000,
            1000,
            OutputType::Dense,
        );
        solver
            .integrate()
            .map_err(|e| anyhow::anyhow!("DOP853 failed: {e}"))?;
        let expected = (end / step).round() as usize + 1;
        ensure!(
            solver.y_out().len() == expected
                && solver
                    .x_out()
                    .iter()
                    .enumerate()
                    .all(|(i, t)| (*t - i as f64 * step).abs() < 1e-6),
            "Integrator output grid mismatch"
        );
        ensure!(
            solver
                .y_out()
                .iter()
                .all(|v| v.iter().all(|x| x.is_finite())),
            "Nonfinite integrated state"
        );
        Ok(solver
            .y_out()
            .iter()
            .map(|v| v.as_slice().as_chunks::<6>().0.to_vec())
            .collect())
    }
}
struct OrbitSystem<'a> {
    model: &'a Model,
    forces: &'a [[f64; 5]],
}
impl System<f64, DVector<f64>> for OrbitSystem<'_> {
    fn system(&self, t: f64, y: &DVector<f64>, dy: &mut DVector<f64>) {
        let env = self.model.environment.at(t);
        for (i, c) in self.forces.iter().enumerate() {
            let p = i * 6;
            let r = V3::from_row_slice(&y.as_slice()[p..p + 3]);
            let v = V3::from_row_slice(&y.as_slice()[p + 3..p + 6]);
            let a = self.model.acceleration_with(env, r, v, *c);
            for k in 0..3 {
                dy[p + k] = v[k];
                dy[p + 3 + k] = a[k];
            }
        }
    }
}
pub fn least_squares(j: DMatrix<f64>, b: &DVector<f64>, rcond: f64) -> Result<DVector<f64>> {
    let svd = j.svd(true, true);
    let threshold = svd.singular_values.max() * rcond;
    svd.solve(b, threshold)
        .map_err(|e| anyhow::anyhow!("Least-squares solve failed: {e}"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fitted {
    pub state: [f64; 6],
    pub forces: [f64; 5],
    pub rms_m: f64,
    pub iterations: Vec<f64>,
}
pub fn fit(model: &Model, positions: &[V3], iterations: usize) -> Result<Fitted> {
    ensure!(
        positions.len() >= 96 && (1..=8).contains(&iterations),
        "Incomplete fit arc or invalid iteration count"
    );
    let weights = [
        -761. / 280.,
        8.,
        -14.,
        56. / 3.,
        -35. / 2.,
        56. / 5.,
        -14. / 3.,
        8. / 7.,
        -1. / 8.,
    ];
    let velocity = positions[..9]
        .iter()
        .zip(weights)
        .fold(V3::zeros(), |a, (p, w)| a + p * w)
        / 900.;
    let mut state: [f64; 6] = std::array::from_fn(|k| {
        if k < 3 {
            positions[0][k]
        } else {
            velocity[k - 3]
        }
    });
    let mut forces = [-1., 0., 0., 0., 0.];
    let scales = [
        10., 10., 10., 0.001, 0.001, 0.001, 0.01, 0.01, 0.01, 0.01, 0.01,
    ];
    let end = (positions.len() - 1) as f64 * 900.;
    let mut history = Vec::new();
    for _ in 0..iterations {
        let mut clones = vec![state; 12];
        let mut coeff = vec![forces; 12];
        for k in 0..11 {
            if k < 6 {
                clones[k + 1][k] += scales[k];
            } else {
                coeff[k + 1][k - 6] += scales[k];
            }
        }
        let trajectory = model.propagate(&clones, &coeff, end, 900.)?;
        let rows = positions.len() * 3;
        let mut j = DMatrix::zeros(rows + 5, 11);
        let mut residual = DVector::zeros(rows + 5);
        let mut rms = 0.;
        for (i, position) in positions.iter().enumerate() {
            for k in 0..3 {
                let row = i * 3 + k;
                let delta = position[k] - trajectory[i][0][k];
                residual[row] = delta;
                rms += delta * delta;
                for col in 0..11 {
                    j[(row, col)] = trajectory[i][col + 1][k] - trajectory[i][0][k];
                }
            }
        }
        for k in 0..5 {
            j[(rows + k, k + 6)] = scales[k + 6] * 0.01;
            residual[rows + k] = -forces[k] * 0.01;
        }
        let correction = least_squares(j, &residual, 1e-10)?;
        for k in 0..11 {
            if k < 6 {
                state[k] += correction[k] * scales[k];
            } else {
                forces[k - 6] += correction[k] * scales[k];
            }
        }
        history.push((rms / rows as f64).sqrt());
    }
    let fitted = model.propagate(&[state], &[forces], end, 900.)?;
    let rms = (positions
        .iter()
        .zip(fitted)
        .map(|(p, s)| (V3::from_row_slice(&s[0][..3]) - p).norm_squared())
        .sum::<f64>()
        / positions.len() as f64)
        .sqrt();
    ensure!(
        state.iter().chain(forces.iter()).all(|v| v.is_finite()),
        "Nonfinite fitted model"
    );
    Ok(Fitted {
        state,
        forces,
        rms_m: rms,
        iterations: history,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dop853_time_dependent_force_matches_analytic_solution() {
        // y'=sin(t), y(0)=0: y=1-cos(t). This detects a wrong stage time,
        // which an autonomous circular-orbit test cannot detect.
        struct TimeForce;
        impl System<f64, DVector<f64>> for TimeForce {
            fn system(&self, t: f64, _y: &DVector<f64>, dy: &mut DVector<f64>) {
                dy[0] = t.sin();
            }
        }
        let mut solver = Dop853::from_param(
            TimeForce,
            0.,
            100.,
            0.25,
            DVector::zeros(1),
            2e-12,
            1e-12,
            0.9,
            0.,
            0.333,
            6.,
            1.,
            0.,
            100000,
            1000,
            OutputType::Dense,
        );
        solver.integrate().unwrap();
        for (&t, y) in solver.x_out().iter().zip(solver.y_out()) {
            assert!(
                (y[0] - (1. - t.cos())).abs() < 1e-10,
                "Time-dependent integration mismatch at {t}: {}",
                y[0]
            );
        }
    }
    #[test]
    fn dop853_circular_orbit_matches_analytic_solution() {
        struct TwoBody;
        impl System<f64, DVector<f64>> for TwoBody {
            fn system(&self, _t: f64, y: &DVector<f64>, dy: &mut DVector<f64>) {
                let r = V3::from_row_slice(&y.as_slice()[..3]);
                let a = -MU * r / r.norm().powi(3);
                for k in 0..3 {
                    dy[k] = y[k + 3];
                    dy[k + 3] = a[k];
                }
            }
        }
        let radius = 26560000f64;
        let rate = (MU / radius.powi(3)).sqrt();
        let initial = DVector::from_vec(vec![radius, 0., 0., 0., radius * rate, 0.]);
        let mut solver = Dop853::from_param(
            TwoBody,
            0.,
            17. * DAY,
            300.,
            initial,
            2e-12,
            1e-8,
            0.9,
            0.,
            0.333,
            6.,
            300.,
            0.,
            100000,
            1000,
            OutputType::Dense,
        );
        solver.integrate().unwrap();
        let error = solver
            .x_out()
            .iter()
            .zip(solver.y_out())
            .map(|(&t, state)| {
                let expected = V3::new(radius * (t * rate).cos(), radius * (t * rate).sin(), 0.);
                (V3::from_row_slice(&state.as_slice()[..3]) - expected).norm()
            })
            .fold(0., f64::max);
        assert!(error < 0.02, "Circular orbit drift {error} m");
    }
}
