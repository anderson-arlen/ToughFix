// SPDX-License-Identifier: MIT
use super::{
    data::{M3, V3},
    physics::least_squares,
};
use anyhow::{Result, ensure};
use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt};
use nalgebra::{DMatrix, DVector, Dyn, storage::Owned};
use std::f64::consts::{PI, TAU};
const MU: f64 = 398600500000000.;
const OMEGA: f64 = 7.2921151467e-5;
const WIDTHS: [u32; 4] = [34, 34, 24, 20];
const RESIDUAL: [u32; 15] = [16, 16, 16, 16, 24, 24, 12, 16, 12, 12, 12, 12, 12, 12, 12];
const FIT: [f64; 15] = [
    1e-3, 1e-7, 1e-7, 1e-7, 1e-7, 1e-7, 1e-11, 1e-11, 1e-11, 1., 1., 1e-7, 1e-7, 1e-7, 1e-7,
];
const POWERS: [i32; 15] = [
    -19, -33, -31, -31, -31, -31, -43, -43, -43, -5, -5, -29, -29, -29, -29,
];
pub fn theta(t: f64) -> f64 {
    let d = (t - 630763200.) / 86400.;
    ((-9.253097568194336e-24 * d + 5.0752099941135916e-15) * d + 6.300388098984894) * d
        + 4.894961212823059
}
pub fn pseudo(t: f64, frame: M3) -> M3 {
    let (s, c) = theta(t).sin_cos();
    M3::new(c, -s, 0., s, c, 0., 0., 0., 1.) * frame
}
pub fn position(p: &[f64; 15], reference: f64, t: f64) -> Option<V3> {
    let [
        root,
        e,
        inc,
        node,
        m0,
        w,
        idot,
        odot,
        dn,
        crc,
        crs,
        cic,
        cis,
        cuc,
        cus,
    ] = *p;
    let a = root * root;
    if a <= 0. || !(0. ..1.).contains(&e) {
        return None;
    }
    let dt = t - reference;
    let mean = m0 + ((MU / a.powi(3)).sqrt() + dn) * dt;
    let mut eccentric = mean;
    for _ in 0..10 {
        eccentric -= (eccentric - e * eccentric.sin() - mean) / (1. - e * eccentric.cos());
    }
    let true_anomaly = ((1. - e * e).sqrt() * eccentric.sin()).atan2(eccentric.cos() - e);
    let phi = true_anomaly + w;
    let (s, c) = (2. * phi).sin_cos();
    let u = phi + cus * s + cuc * c;
    let radius = a * (1. - e * eccentric.cos()) + crs * s + crc * c;
    let tilt = inc + idot * dt + cis * s + cic * c;
    let omega = node - theta(reference) + (odot - OMEGA) * dt;
    let x = radius * u.cos();
    let y = radius * u.sin();
    let result = V3::new(
        x * omega.cos() - y * tilt.cos() * omega.sin(),
        x * omega.sin() + y * tilt.cos() * omega.cos(),
        y * tilt.sin(),
    );
    result.iter().all(|v| v.is_finite()).then_some(result)
}
pub fn kepler(r: V3, v: V3) -> [f64; 15] {
    let h = r.cross(&v);
    let normal = h.normalize();
    let n = V3::z().cross(&normal).normalize();
    let ev = v.cross(&h) / MU - r / r.norm();
    let e = ev.norm();
    let a = 1. / (2. / r.norm() - v.norm_squared() / MU);
    let w = n.cross(&ev).dot(&normal).atan2(n.dot(&ev));
    let true_anomaly = ev.cross(&r).dot(&normal).atan2(ev.dot(&r));
    let eccentric = ((1. - e * e).sqrt() * true_anomaly.sin()).atan2(e + true_anomaly.cos());
    let mut p = [0.; 15];
    p[..6].copy_from_slice(&[
        a.sqrt(),
        e,
        normal.z.acos(),
        n.y.atan2(n.x),
        eccentric - e * eccentric.sin(),
        w,
    ]);
    p
}
struct Problem<'a> {
    initial: [f64; 15],
    reference: f64,
    times: &'a [f64],
    targets: &'a [V3],
    z: DVector<f64>,
}
impl Problem<'_> {
    fn parameters(&self) -> [f64; 15] {
        std::array::from_fn(|k| self.initial[k] + self.z[k] * FIT[k])
    }
}
impl LeastSquaresProblem<f64, Dyn, Dyn> for Problem<'_> {
    type ResidualStorage = Owned<f64, Dyn>;
    type JacobianStorage = Owned<f64, Dyn, Dyn>;
    type ParameterStorage = Owned<f64, Dyn>;
    fn set_params(&mut self, x: &DVector<f64>) {
        self.z.copy_from(x)
    }
    fn params(&self) -> DVector<f64> {
        self.z.clone()
    }
    fn residuals(&self) -> Option<DVector<f64>> {
        let p = self.parameters();
        let mut r = DVector::zeros(self.times.len() * 3);
        for (i, (&t, target)) in self.times.iter().zip(self.targets).enumerate() {
            let delta = position(&p, self.reference, t)? - target;
            for k in 0..3 {
                r[i * 3 + k] = delta[k];
            }
        }
        Some(r)
    }
    fn jacobian(&self) -> Option<DMatrix<f64>> {
        let baseline = self.residuals()?;
        let mut j = DMatrix::zeros(baseline.len(), 15);
        let p = self.parameters();
        for k in 0..15 {
            let mut shifted = p;
            shifted[k] += 0.01 * FIT[k];
            for (i, &t) in self.times.iter().enumerate() {
                let delta = position(&shifted, self.reference, t)? - self.targets[i];
                for axis in 0..3 {
                    j[(i * 3 + axis, k)] = (delta[axis] - baseline[i * 3 + axis]) / 0.01;
                }
            }
        }
        Some(j)
    }
}
pub fn fit(initial: [f64; 15], reference: f64, times: &[f64], targets: &[V3]) -> Result<[f64; 15]> {
    let problem = Problem {
        initial,
        reference,
        times,
        targets,
        z: DVector::zeros(15),
    };
    let (problem, report) = LevenbergMarquardt::new()
        .with_ftol(1e-10)
        .with_xtol(1e-10)
        .with_gtol(1e-5)
        .with_patience(10)
        .minimize(problem);
    ensure!(
        report.termination.was_successful(),
        "Broadcast fit failed: {:?}",
        report.termination
    );
    Ok(problem.parameters())
}
/// Stable polynomial fit on [-1,1], converted to powers of the original x.
pub fn polynomial(x: &[f64], y: &[f64], degree: usize) -> Result<Vec<f64>> {
    ensure!(
        x.len() == y.len() && x.len() > degree,
        "Insufficient polynomial samples"
    );
    let center = (x[0] + x[x.len() - 1]) / 2.;
    let half = (x[x.len() - 1] - x[0]) / 2.;
    ensure!(half > 0., "Degenerate polynomial domain");
    let matrix = DMatrix::from_fn(x.len(), degree + 1, |i, k| {
        ((x[i] - center) / half).powi(k as i32)
    });
    let beta = least_squares(matrix, &DVector::from_column_slice(y), 1e-14)?;
    let mut result = vec![0.; degree + 1];
    let mut power = vec![1.];
    for k in 0..=degree {
        for (j, &v) in power.iter().enumerate() {
            result[j] += beta[k] * v;
        }
        let mut next = vec![0.; power.len() + 1];
        for (j, &v) in power.iter().enumerate() {
            next[j] -= v * center / half;
            next[j + 1] += v / half;
        }
        power = next;
    }
    Ok(result)
}
#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    used: usize,
}
impl Bits {
    fn put(&mut self, value: i64, width: u32, signed: bool) -> Result<()> {
        let (low, high) = if signed {
            (-(1i64 << (width - 1)), (1i64 << (width - 1)) - 1)
        } else {
            (0, (1i64 << width) - 1)
        };
        ensure!(
            (low..=high).contains(&value),
            "{value} overflows {width}-bit field"
        );
        for k in (0..width).rev() {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let at = self.bytes.len() - 1;
            self.bytes[at] |= (((value >> k) & 1) as u8) << (7 - self.used % 8);
            self.used += 1;
        }
        Ok(())
    }
}
pub fn unavailable() -> Vec<u8> {
    let mut r = vec![0; 1021];
    r[1] = 255;
    r
}
pub fn encode(prn: u8, parameters: &[[f64; 15]], clock: [f64; 3]) -> Result<Vec<u8>> {
    ensure!(
        (1..=32).contains(&prn)
            && parameters.len() == 28
            && parameters
                .iter()
                .flatten()
                .chain(clock.iter())
                .all(|v| v.is_finite()),
        "Invalid ephemerides"
    );
    let mut p = parameters.to_vec();
    for k in [3, 5] {
        for j in 1..28 {
            p[j][k] += TAU * ((p[j - 1][k] - p[j][k]) / TAU).round_ties_even();
        }
    }
    for j in 1..28 {
        let expected = p[j - 1][4] + (MU / p[j - 1][0].powi(6)).sqrt() * 21600.;
        p[j][4] += TAU * ((expected - p[j][4]) / TAU).round_ties_even();
    }
    for k in [3, 4, 5] {
        let shift = TAU * ((p[0][k] + PI) / TAU).floor();
        for row in &mut p {
            row[k] -= shift;
        }
    }
    for row in &mut p {
        for v in &mut row[2..9] {
            *v /= PI;
        }
    }
    let mut bits = Bits::default();
    bits.put(prn as i64, 8, false)?;
    bits.put(0, 8, false)?;
    for ((v, power), width) in clock.into_iter().zip([-40, -50, -64]).zip([32, 24, 16]) {
        bits.put((v / 2f64.powi(power)).round_ties_even() as i64, width, true)?;
    }
    bits.put(0, 8, false)?;
    let x: Vec<_> = (0..28).map(|j| j as f64).collect();
    for k in 0..15 {
        let raw: Vec<_> = p.iter().map(|r| r[k] / 2f64.powi(POWERS[k])).collect();
        let mut poly = polynomial(&x, &raw, 3)?;
        for degree in (1..=3).rev() {
            let factor = if degree == 3 { 256. } else { 1. };
            let q = (poly[degree] * factor).round_ties_even();
            let limit = (1i64 << (WIDTHS[degree] - 1)) as f64;
            let bound = q.clamp(-limit, limit - 1.);
            if q != bound {
                poly[degree] = bound / factor;
                let target: Vec<_> = x
                    .iter()
                    .zip(&raw)
                    .map(|(&t, &v)| {
                        v - (degree..4).map(|d| poly[d] * t.powi(d as i32)).sum::<f64>()
                    })
                    .collect();
                let lower = polynomial(&x, &target, degree - 1)?;
                poly[..degree].copy_from_slice(&lower);
            }
        }
        let q: [i64; 4] = std::array::from_fn(|d| {
            (poly[d] * if d == 3 { 256. } else { 1. }).round_ties_even() as i64
        });
        for (v, width) in q.into_iter().zip(WIDTHS) {
            bits.put(v, width, true)?;
        }
        bits.put(0, 4, true)?;
        bits.put(0, 4, true)?;
        for (&t, &v) in x.iter().zip(&raw) {
            let trend =
                ((q[3] as f64 / 256. * t + q[2] as f64) * t + q[1] as f64) * t + q[0] as f64;
            bits.put((v - trend).round_ties_even() as i64, RESIDUAL[k], true)?;
        }
    }
    ensure!(bits.used == 1021 * 8, "CEP record length mismatch");
    Ok(bits.bytes)
}
pub fn archive(start: f64, weeks: &[Vec<Vec<u8>>]) -> Result<Vec<u8>> {
    ensure!(
        weeks.len() == 4
            && start.fract() == 0.
            && start >= 0.
            && start + 3. * 604800. < u32::MAX as f64,
        "Invalid archive epoch or block count"
    );
    let mut output = Vec::new();
    for (week, records) in weeks.iter().enumerate() {
        ensure!(records.len() == 32, "Invalid slot count");
        let at = output.len();
        output.extend_from_slice(&((start + week as f64 * 604800.) as u32).to_be_bytes());
        output.extend_from_slice(&[week as u8, 4]);
        for (slot, r) in records.iter().enumerate() {
            ensure!(
                r.len() == 1021 && (r[0] == slot as u8 + 1 || (r[0] == 0 && r[1] != 0)),
                "Invalid CEP slot"
            );
            output.extend(r);
        }
        let mut crc = 0u16;
        for &byte in &output[at..] {
            crc ^= (byte as u16) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x1021
                } else {
                    crc << 1
                };
            }
        }
        output.extend_from_slice(&crc.to_be_bytes());
    }
    Ok(output)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matches_independent_python_encoder_fixture() {
        #[derive(serde::Deserialize)]
        struct Fixture {
            prn: u8,
            parameters: Vec<[f64; 15]>,
            clock: [f64; 3],
            record_hex: String,
        }
        let fixture: Fixture =
            serde_json::from_str(include_str!("../../tests/fixtures/cep-encoder.json")).unwrap();
        let record = encode(fixture.prn, &fixture.parameters, fixture.clock).unwrap();
        let expected: Vec<_> = fixture
            .record_hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
            .collect();
        assert_eq!(record, expected);
    }
    #[test]
    fn strict_field_bounds() {
        let mut b = Bits::default();
        assert!(b.put(128, 8, true).is_err());
        assert!(b.put(-129, 8, true).is_err());
        b.put(-128, 8, true).unwrap();
        assert_eq!(b.bytes, [128]);
    }
    #[test]
    fn constrained_polynomial_reproduces_cubic() {
        let x: Vec<_> = (0..28).map(|i| i as f64).collect();
        let y: Vec<_> = x
            .iter()
            .map(|t| 1e9 + 3e5 * t - 20. * t * t + 0.5 * t * t * t)
            .collect();
        let p = polynomial(&x, &y, 3).unwrap();
        for (a, b) in p.iter().zip([1e9, 3e5, -20., 0.5]) {
            assert!((a - b).abs() < 1e-5);
        }
    }
}
