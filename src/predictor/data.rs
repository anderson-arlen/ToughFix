// SPDX-License-Identifier: MIT
use anyhow::{Context, Result, ensure};
use chrono::{NaiveDate, NaiveDateTime};
use flate2::read::GzDecoder;
use nalgebra::{Matrix3, Vector3};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};
pub type V3 = Vector3<f64>;
pub type M3 = Matrix3<f64>;
pub const DAY: f64 = 86400.;
pub const GPS_JD: f64 = 2444244.5;
pub const AU: f64 = 149597870700.;
pub const MU: f64 = 3.986004415e14;
pub const R_EARTH: f64 = 6378136.3;

pub fn epoch(text: &str) -> Result<f64> {
    let t = NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .context("Use a GPS calendar timestamp YYYY-MM-DDTHH:MM:SS")?;
    Ok(t.and_utc().timestamp() as f64 - 315964800.)
}
pub fn calendar(t: f64) -> String {
    chrono::DateTime::from_timestamp((t + 315964800.) as i64, 0)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub time: f64,
    pub positions: Vec<Option<[f64; 3]>>,
    pub clocks: Vec<Option<f64>>,
}
pub struct Observations {
    pub samples: Vec<Sample>,
    pub paths: Vec<PathBuf>,
}
pub fn observations(dir: &Path) -> Result<Observations> {
    observations_with_ultra(dir, false)
}
pub fn observations_with_ultra(dir: &Path, include_ultra: bool) -> Result<Observations> {
    let mut paths: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name().and_then(|s| s.to_str()).is_some_and(|s| {
                (s.starts_with("IGS0OPSRAP_") && s.ends_with(".SP3.gz"))
                    || (include_ultra && s.starts_with("igu") && s.ends_with(".sp3.gz"))
            })
        })
        .collect();
    // Later records win: prefer Rapid on overlapping epochs, and newer Ultra
    // products over older Ultra products. Never splice predicted halves.
    paths.sort_by_key(|p| {
        let name = p.file_name().unwrap().to_string_lossy();
        (name.starts_with("IGS0OPSRAP_"), name.into_owned())
    });
    ensure!(
        !paths.is_empty(),
        "No observed IGS rapid SP3 files in {}",
        dir.display()
    );
    let mut samples: BTreeMap<i64, Sample> = BTreeMap::new();
    for path in &paths {
        let name = path.file_name().unwrap().to_string_lossy();
        let ultra_end = if name.starts_with("igu") {
            Some(super::sources::ultra_epoch(&name)? as f64)
        } else {
            None
        };
        let mut raw = String::new();
        GzDecoder::new(File::open(path)?)
            .take(32 * 1024 * 1024)
            .read_to_string(&mut raw)?;
        ensure!(
            raw.lines()
                .take(22)
                .any(|l| l.starts_with("%c") && l.split_whitespace().any(|s| s == "GPS")),
            "SP3 does not declare GPS time: {}",
            path.display()
        );
        let mut time = None;
        for line in raw.lines() {
            if let Some(epoch) = line.strip_prefix("* ") {
                let parts: Vec<_> = epoch.split_whitespace().collect();
                ensure!(parts.len() == 6, "Invalid SP3 epoch");
                let date = NaiveDate::from_ymd_opt(
                    parts[0].parse()?,
                    parts[1].parse()?,
                    parts[2].parse()?,
                )
                .context("Invalid SP3 date")?;
                let sec: f64 = parts[5].parse()?;
                ensure!(
                    sec.is_finite() && sec.fract() == 0.,
                    "Nonintegral SP3 epoch"
                );
                let t = date
                    .and_hms_opt(parts[3].parse()?, parts[4].parse()?, sec as u32)
                    .context("Invalid SP3 time")?
                    .and_utc()
                    .timestamp()
                    - 315964800;
                time = Some(t);
                if ultra_end.is_some_and(|end| t as f64 >= end || (t as f64) < end - DAY) {
                    continue;
                }
                samples.entry(t).or_insert_with(|| Sample {
                    time: t as f64,
                    positions: vec![None; 32],
                    clocks: vec![None; 32],
                });
            } else if line.starts_with("PG") {
                ensure!(
                    line.is_ascii() && line.len() >= 60,
                    "Truncated SP3 GPS position"
                );
                let prn: usize = line[2..4].trim().parse()?;
                if !(1..=32).contains(&prn) {
                    continue;
                }
                let t = time.context("Position preceding SP3 epoch")?;
                if ultra_end.is_some_and(|end| t as f64 >= end || (t as f64) < end - DAY) {
                    continue;
                }
                let sample = samples.get_mut(&t).unwrap();
                if line.as_bytes().get(79) != Some(&b'P') {
                    let xyz: [f64; 3] = [
                        line[4..18].trim().parse::<f64>()? * 1000.,
                        line[18..32].trim().parse::<f64>()? * 1000.,
                        line[32..46].trim().parse::<f64>()? * 1000.,
                    ];
                    if xyz.iter().all(|v| v.is_finite() && v.abs() < 1e9)
                        && V3::from(xyz).norm() > 1e6
                    {
                        sample.positions[prn - 1] = Some(xyz);
                    }
                }
                if line.as_bytes().get(75) != Some(&b'P') {
                    let clock: f64 = line[46..60].trim().parse()?;
                    if clock.is_finite() && clock.abs() < 999999. {
                        sample.clocks[prn - 1] = Some(clock * 1e-6);
                    }
                }
            }
        }
    }
    Ok(Observations {
        samples: samples
            .into_values()
            .filter(|s| s.positions.iter().any(Option::is_some))
            .collect(),
        paths,
    })
}

/// Latest completed observed interval, excluding cached future samples.
pub fn latest_cutoff(observed: &Observations, now: f64) -> Result<f64> {
    let latest = observed
        .samples
        .iter()
        .rfind(|s| s.time + 900. <= now)
        .context("No completed observed GPS intervals")?
        .time;
    ensure!(
        now - latest <= 48. * 3600.,
        "Fitting observations are older than 48 hours"
    );
    Ok(latest + 900.)
}

pub fn file_interval(path: &Path) -> Result<(f64, f64)> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("Invalid SP3 filename")?;
    if name.starts_with("igu") {
        let end = super::sources::ultra_epoch(name)? as f64;
        return Ok((end - DAY, end));
    }
    ensure!(name.is_ascii() && name.len() > 18, "Invalid Rapid filename");
    let date = NaiveDate::from_yo_opt(name[11..15].parse()?, name[15..18].parse()?)
        .context("Invalid Rapid date")?;
    let start = date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp() as f64 - 315964800.;
    Ok((start, start + DAY))
}

#[derive(Clone)]
pub struct Frames {
    eop: Vec<[f64; 4]>,
}
impl Frames {
    pub fn read(path: &Path) -> Result<Self> {
        let mut eop = Vec::new();
        for line in fs::read_to_string(path)?.lines() {
            if !line.is_ascii() || line.len() < 68 {
                continue;
            }
            let values = [&line[7..15], &line[18..27], &line[37..46], &line[58..68]]
                .map(|s| s.trim().parse::<f64>());
            if let [Ok(mjd), Ok(xp), Ok(yp), Ok(dut)] = values
                && [mjd, xp, yp, dut].iter().all(|x| x.is_finite())
            {
                eop.push([mjd, xp, yp, dut]);
            }
        }
        ensure!(
            eop.len() > 2 && eop.windows(2).all(|p| p[0][0] < p[1][0]),
            "Invalid Earth orientation table"
        );
        Ok(Self { eop })
    }
    pub fn matrix(&self, t: f64) -> Result<M3> {
        let ((utc1, utc2), _) = erfars::timescales::Taiutc(GPS_JD, (t + 19.) / DAY)
            .map_err(|e| anyhow::anyhow!("ERFA TAI/UTC: {e:?}"))?;
        let mjd = utc1 - 2400000.5 + utc2;
        ensure!(
            mjd >= self.eop[0][0] && mjd <= self.eop.last().unwrap()[0],
            "Earth orientation does not cover prediction epoch"
        );
        let at = self
            .eop
            .partition_point(|r| r[0] < mjd)
            .clamp(1, self.eop.len() - 1);
        let a = self.eop[at - 1];
        let b = self.eop[at];
        let fraction = (mjd - a[0]) / (b[0] - a[0]);
        let v = [1, 2, 3].map(|k| a[k] + fraction * (b[k] - a[k]));
        let arcsec = std::f64::consts::PI / (180. * 3600.);
        let mut matrix = [0.; 9];
        erfars::precnutpolar::C2t06a(
            GPS_JD,
            (t + 51.184) / DAY,
            utc1,
            utc2 + v[2] / DAY,
            v[0] * arcsec,
            v[1] * arcsec,
            &mut matrix,
        );
        Ok(M3::from_row_slice(&matrix))
    }
}

/// Uniform not-a-knot cubic spline, the same boundary conditions as SciPy.
#[derive(Clone)]
pub struct Spline<const N: usize> {
    start: f64,
    step: f64,
    coeff: Vec<[[f64; N]; 4]>,
}
impl<const N: usize> Spline<N> {
    pub fn new(start: f64, step: f64, y: &[[f64; N]]) -> Result<Self> {
        ensure!(
            y.len() >= 4 && step > 0.,
            "Cubic spline needs four regular samples"
        );
        let n = y.len();
        let mut m = vec![[0.; N]; n];
        for k in 0..N {
            let rhs: Vec<_> = (1..n - 1)
                .map(|i| 6. * (y[i + 1][k] - 2. * y[i][k] + y[i - 1][k]) / (step * step))
                .collect();
            m[1][k] = rhs[0] / 6.;
            m[n - 2][k] = rhs[n - 3] / 6.;
            let count = n - 4;
            let mut upper = vec![0.; count];
            let mut target = vec![0.; count];
            for j in 0..count {
                let mut r = rhs[j + 1];
                if j == 0 {
                    r -= m[1][k];
                }
                if j + 1 == count {
                    r -= m[n - 2][k];
                }
                let denominator = 4. - if j > 0 { upper[j - 1] } else { 0. };
                upper[j] = if j + 1 < count { 1. / denominator } else { 0. };
                target[j] = (r - if j > 0 { target[j - 1] } else { 0. }) / denominator;
            }
            for j in (0..count).rev() {
                m[j + 2][k] = target[j]
                    - if j + 1 < count {
                        upper[j] * m[j + 3][k]
                    } else {
                        0.
                    };
            }
            m[0][k] = 2. * m[1][k] - m[2][k];
            m[n - 1][k] = 2. * m[n - 2][k] - m[n - 3][k];
        }
        let coeff = (0..n - 1)
            .map(|i| {
                std::array::from_fn(|degree| {
                    std::array::from_fn(|k| match degree {
                        0 => y[i][k],
                        1 => {
                            (y[i + 1][k] - y[i][k]) / step
                                - step * (2. * m[i][k] + m[i + 1][k]) / 6.
                        }
                        2 => m[i][k] / 2.,
                        _ => (m[i + 1][k] - m[i][k]) / (6. * step),
                    })
                })
            })
            .collect();
        Ok(Self { start, step, coeff })
    }
    pub fn at(&self, t: f64) -> [f64; N] {
        let i = (((t - self.start) / self.step).floor().max(0.) as usize).min(self.coeff.len() - 1);
        let dt = t - self.start - i as f64 * self.step;
        let c = &self.coeff[i];
        std::array::from_fn(|k| ((c[3][k] * dt + c[2][k]) * dt + c[1][k]) * dt + c[0][k])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn observed_ultra_half_is_enforced_even_without_prediction_flags_and_rapid_wins() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let temp = crate::test_support::Temp::new();
        let end = super::super::sources::ultra_epoch("igu24383_12.sp3.gz").unwrap() as f64;
        let make = |path: &str, xyz: f64, epochs: &[f64], predicted: bool| {
            let mut text = String::from("%c cc GPS\n");
            for &time in epochs {
                let d = chrono::DateTime::from_timestamp((time + 315964800.) as i64, 0).unwrap();
                text.push_str(&format!("*  {}\n", d.format("%Y %m %d %H %M 00.00000000")));
                let mut line = format!(
                    "PG01{:14.6}{:14.6}{:14.6}{:14.6}                    ",
                    xyz, 10000., 10000., 1.
                )
                .into_bytes();
                if predicted {
                    line[75] = b'P';
                    line[79] = b'P';
                }
                text.push_str(&String::from_utf8(line).unwrap());
                text.push('\n');
            }
            let mut gzip = GzEncoder::new(
                File::create(temp.path().join(path)).unwrap(),
                Compression::default(),
            );
            gzip.write_all(text.as_bytes()).unwrap();
            gzip.finish().unwrap();
        };
        make(
            "igu24383_12.sp3.gz",
            22000.,
            &[end - 900., end, end + 900.],
            false,
        );
        let parsed = observations_with_ultra(temp.path(), true).unwrap();
        assert_eq!(parsed.samples.len(), 1);
        assert_eq!(parsed.samples[0].time, end - 900.);
        make("igu24383_06.sp3.gz", 24000., &[end - 7. * 3600.], true);
        make(
            "IGS0OPSRAP_20262730000_01D_15M_ORB.SP3.gz",
            23000.,
            &[end - 900.],
            false,
        );
        let parsed = observations_with_ultra(temp.path(), true).unwrap();
        assert_eq!(parsed.samples.len(), 1);
        assert_eq!(parsed.samples[0].positions[0].unwrap()[0], 23000000.);
    }
    #[test]
    fn cutoff_accepts_normal_rapid_latency_but_rejects_stale_and_future_intervals() {
        let sample = Sample {
            time: 1000000.,
            positions: vec![Some([26000000., 0., 0.]); 32],
            clocks: vec![None; 32],
        };
        let observed = Observations {
            samples: vec![sample],
            paths: vec![],
        };
        assert!(latest_cutoff(&observed, 1000000. + 41. * 3600.).is_ok());
        assert!(latest_cutoff(&observed, 1000000. + 49. * 3600.).is_err());
        assert!(latest_cutoff(&observed, 1000000. + 899.).is_err());
    }

    use super::*;
    #[test]
    fn spline_matches_scipy_nonpolynomial_fixture() {
        let values: Vec<_> = (0..40).map(|i| [(i as f64 * 0.3).sin()]).collect();
        let spline = Spline::new(-600., 600., &values).unwrap();
        let expected = [0.3538698664155746, 0.9618725712332893, 0.4105252661230797];
        for (t, value) in [123.456, 1987.65, 12812.45].into_iter().zip(expected) {
            assert!(
                (spline.at(t)[0] - value).abs() < 1e-13,
                "{t}: {} vs {value}",
                spline.at(t)[0]
            );
        }
    }
    #[test]
    fn not_a_knot_reproduces_a_cubic() {
        let data: Vec<_> = (0..9)
            .map(|i| {
                let x = i as f64;
                [x * x * x - 2. * x * x + 3. * x - 1.]
            })
            .collect();
        let spline = Spline::new(0., 1., &data).unwrap();
        for i in 0..80 {
            let x = i as f64 / 10.;
            assert!((spline.at(x)[0] - (x * x * x - 2. * x * x + 3. * x - 1.)).abs() < 1e-10)
        }
    }
}
