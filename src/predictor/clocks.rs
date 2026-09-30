// SPDX-License-Identifier: MIT
use super::{
    cep::polynomial,
    data::{DAY, Sample},
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
#[derive(Serialize)]
pub struct Clock {
    pub coefficients: [f64; 3],
    pub samples: usize,
    pub degree: usize,
    pub training_rms_ns: f64,
    pub largest_step_ns: f64,
    pub past_day_validation_ns: [f64; 2],
    pub source: String,
    pub training_start_gps_seconds: f64,
    pub training_end_gps_seconds: f64,
    pub selection_reason: String,
}
fn evaluate(beta: &[f64], x: f64) -> f64 {
    beta.iter().rev().fold(0., |a, b| a * x + b)
}
pub fn fit(samples: &[&Sample], prn: usize, cutoff: f64) -> Result<Clock> {
    let values: Vec<_> = samples
        .iter()
        .filter(|s| s.time >= cutoff - 3. * DAY && s.time < cutoff)
        .filter_map(|s| s.clocks[prn - 1].map(|v| (s.time, v)))
        .collect();
    ensure!(
        values.len() >= 240
            && values.last().unwrap().0 >= cutoff - 1800.
            && values.windows(2).all(|w| w[1].0 - w[0].0 <= 3600.),
        "Insufficient recent observed clock coverage"
    );
    let first = values[0].0;
    let baseline = values[0].1;
    let x: Vec<_> = values.iter().map(|v| (v.0 - first) / DAY).collect();
    let y: Vec<_> = values.iter().map(|v| v.1 - baseline).collect();
    let split = values.partition_point(|v| v.0 < cutoff - DAY);
    ensure!(
        split >= 3 && split < values.len(),
        "Missing clock validation split"
    );
    let mut scores = [0.; 2];
    for d in 1..=2 {
        let beta = polynomial(&x[..split], &y[..split], d)?;
        scores[d - 1] = (x[split..]
            .iter()
            .zip(&y[split..])
            .map(|(&t, &v)| (evaluate(&beta, t) - v).powi(2))
            .sum::<f64>()
            / (x.len() - split) as f64)
            .sqrt();
    }
    let degree = if scores[1] < 0.8 * scores[0] && scores[0] - scores[1] > 0.2e-9 {
        2
    } else {
        1
    };
    let mut beta = [0.; 3];
    beta[..=degree].copy_from_slice(&polynomial(&x, &y, degree)?);
    let error: Vec<_> = x
        .iter()
        .zip(&y)
        .map(|(&t, &v)| v - evaluate(&beta, t))
        .collect();
    let rms = (error.iter().map(|v| v * v).sum::<f64>() / error.len() as f64).sqrt();
    let jump = error
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0., f64::max);
    ensure!(
        rms <= 50e-9 && jump <= 50e-9,
        "Unstable clock: RMS {:.1} ns, step {:.1} ns",
        rms * 1e9,
        jump * 1e9
    );
    let at = (cutoff + 10800. - first) / DAY;
    Ok(Clock {
        coefficients: [
            baseline + evaluate(&beta, at),
            (beta[1] + 2. * beta[2] * at) / DAY,
            beta[2] / DAY.powi(2),
        ],
        samples: values.len(),
        degree,
        training_rms_ns: rms * 1e9,
        largest_step_ns: jump * 1e9,
        past_day_validation_ns: scores.map(|v| v * 1e9),
        source: "rapid".into(),
        training_start_gps_seconds: cutoff - 3. * DAY,
        training_end_gps_seconds: values.last().unwrap().0,
        selection_reason: "Rapid observed clock fit".into(),
    })
}

/// Use the newer observed clocks only when their past-data residuals are
/// reasonably quiet relative to the available Rapid fit. Thresholds are a
/// conservative quality heuristic, not a guarantee about future clock error.
pub fn select(train: &[&Sample], rapid: &[Sample], prn: usize, cutoff: f64) -> Result<Clock> {
    let rapid_cutoff = rapid
        .iter()
        .rfind(|s| s.time < cutoff)
        .map(|s| s.time + 900.);
    let fallback = rapid_cutoff
        .filter(|&t| cutoff - (t - 900.) <= 48. * 3600.)
        .and_then(|t| {
            let samples: Vec<_> = rapid
                .iter()
                .filter(|s| s.time >= t - 3. * DAY && s.time < t)
                .collect();
            fit(&samples, prn, t).ok().map(|mut c| {
                c.coefficients = shift(c.coefficients, cutoff - t);
                c
            })
        });
    if rapid_cutoff == Some(cutoff) {
        return fallback.context("No valid Rapid clock fit");
    }
    let candidate = fit(train, prn, cutoff);
    if let Ok(mut newer) = candidate {
        let rms_limit = fallback
            .as_ref()
            .map_or(1., |r| (2. * r.training_rms_ns).max(1.));
        let step_limit = fallback
            .as_ref()
            .map_or(5., |r| (4. * r.largest_step_ns).max(5.));
        if newer.training_rms_ns <= rms_limit && newer.largest_step_ns <= step_limit {
            newer.source = "rapid+observed-ultra".into();
            newer.selection_reason = format!(
                "Newer observed clocks passed residual gates: RMS <= {rms_limit:.3} ns; step <= {step_limit:.3} ns"
            );
            return Ok(newer);
        }
        if let Some(mut older) = fallback {
            older.selection_reason = format!(
                "Rapid fallback: newer RMS {:.3} ns / step {:.3} ns exceeded limits {:.3} / {:.3} ns",
                newer.training_rms_ns, newer.largest_step_ns, rms_limit, step_limit
            );
            return Ok(older);
        }
        anyhow::bail!(
            "Newer observed clocks failed quality gates and no recent Rapid fallback exists"
        );
    }
    fallback
        .map(|mut c| {
            c.selection_reason = "Rapid fallback: newer clock fit unavailable or unstable".into();
            c
        })
        .context("Neither newer nor recent Rapid clocks have a usable fit")
}
pub fn shift([a, b, c]: [f64; 3], t: f64) -> [f64; 3] {
    [a + b * t + c * t * t, b + 2. * c * t, c]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn samples(cutoff: f64, noisy: bool) -> Vec<Sample> {
        (0..288)
            .map(|i| {
                let time = cutoff - 3. * DAY + i as f64 * 900.;
                let noise = if noisy && i >= 192 {
                    if i % 2 == 0 { 8e-9 } else { -8e-9 }
                } else {
                    0.
                };
                Sample {
                    time,
                    positions: vec![Some([26000000., 0., 0.]); 32],
                    clocks: vec![Some(1e-4 + time * 1e-13 + noise); 32],
                }
            })
            .collect()
    }
    #[test]
    fn noisy_newer_clocks_fall_back_without_changing_reference_epoch() {
        let cutoff = 10. * DAY;
        let rapid = samples(cutoff - DAY, false);
        let combined = samples(cutoff, true);
        let refs: Vec<_> = combined.iter().collect();
        let selected = select(&refs, &rapid, 1, cutoff).unwrap();
        assert_eq!(selected.source, "rapid");
        assert_eq!(selected.training_start_gps_seconds, cutoff - 4. * DAY);
        assert!(selected.selection_reason.contains("exceeded"));
        let original = fit(&rapid.iter().collect::<Vec<_>>(), 1, cutoff - DAY).unwrap();
        for elapsed in [0., DAY, 14. * DAY] {
            assert!(
                (evaluate(&selected.coefficients, elapsed)
                    - evaluate(&original.coefficients, elapsed + DAY))
                .abs()
                    < 1e-15
            );
        }
    }
    #[test]
    fn quiet_newer_clocks_pass_and_stale_fallback_cannot_rescue_bad_clocks() {
        let cutoff = 10. * DAY;
        let rapid = samples(cutoff - DAY, false);
        let combined = samples(cutoff, false);
        let refs: Vec<_> = combined.iter().collect();
        let selected = select(&refs, &rapid, 1, cutoff).unwrap();
        assert_eq!(selected.source, "rapid+observed-ultra");
        assert_eq!(selected.training_start_gps_seconds, cutoff - 3. * DAY);
        let stale = samples(cutoff - 3. * DAY, false);
        let noisy = samples(cutoff, true);
        assert!(select(&noisy.iter().collect::<Vec<_>>(), &stale, 1, cutoff).is_err());
        assert!(select(&refs, &[], 1, cutoff).is_ok());
    }
    #[test]
    fn future_clock_samples_are_never_used() {
        let cutoff = 10. * DAY;
        let mut raw = samples(cutoff, false);
        let expected = fit(&raw.iter().collect::<Vec<_>>(), 1, cutoff).unwrap();
        raw.extend(samples(cutoff + 3. * DAY, true));
        let actual = fit(&raw.iter().collect::<Vec<_>>(), 1, cutoff).unwrap();
        assert_eq!(actual.coefficients, expected.coefficients);
        assert_eq!(actual.samples, 288);
    }
}
