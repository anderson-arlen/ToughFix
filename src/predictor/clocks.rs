// SPDX-License-Identifier: MIT
use super::{
    cep::polynomial,
    data::{DAY, Sample},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
#[derive(Deserialize, Serialize)]
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
pub const TG6_FORECAST_DAYS: usize = 14;
const CLOCK_RMS_LIMIT_NS: f64 = 50.;
const TG6_FORECAST_MAX_NS: f64 = 150.;

#[derive(Deserialize, Serialize)]
pub struct ForecastValidation {
    pub forecast_days: usize,
    pub forecast_start_gps_seconds: f64,
    pub training_end_gps_seconds: f64,
    pub observed_end_gps_seconds: f64,
    pub samples: usize,
    pub rms_ns: f64,
    pub max_absolute_ns: f64,
}
#[derive(Deserialize, Serialize)]
pub struct Tg6Clock {
    pub model: Clock,
    pub validation: ForecastValidation,
}
impl Tg6Clock {
    pub fn validate(&self, cutoff: f64) -> Result<()> {
        let c = &self.model;
        ensure!(
            c.degree == 1
                && c.coefficients[2] == 0.
                && c.coefficients.iter().all(|v| v.is_finite()),
            "TG-6 clock must be independently fitted as a finite linear model"
        );
        ensure!(
            c.training_rms_ns <= CLOCK_RMS_LIMIT_NS
                && c.largest_step_ns <= CLOCK_RMS_LIMIT_NS
                && c.past_day_validation_ns[0] <= CLOCK_RMS_LIMIT_NS
                && [
                    c.training_rms_ns,
                    c.largest_step_ns,
                    c.past_day_validation_ns[0]
                ]
                .iter()
                .all(|v| v.is_finite() && *v >= 0.)
                && c.samples >= 240
                && c.training_start_gps_seconds < c.training_end_gps_seconds
                && (900. ..=48. * 3600.).contains(&(cutoff - c.training_end_gps_seconds)),
            "TG-6 linear clock failed recent-data quality checks"
        );
        let v = &self.validation;
        ensure!(
            v.forecast_days == TG6_FORECAST_DAYS
                && v.forecast_start_gps_seconds == cutoff - TG6_FORECAST_DAYS as f64 * DAY
                && (900. ..=1800.)
                    .contains(&(v.forecast_start_gps_seconds - v.training_end_gps_seconds))
                && (900. ..=1800.).contains(&(cutoff - v.observed_end_gps_seconds))
                && v.samples >= TG6_FORECAST_DAYS * 80
                && v.rms_ns.is_finite()
                && (0. ..=CLOCK_RMS_LIMIT_NS).contains(&v.rms_ns)
                && v.max_absolute_ns.is_finite()
                && (v.rms_ns..=TG6_FORECAST_MAX_NS).contains(&v.max_absolute_ns),
            "TG-6 linear clock failed the past 14-day forecast check (RMS {:.1} ns, max {:.1} ns; limits 50/150 ns)",
            v.rms_ns,
            v.max_absolute_ns
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Model {
    Adaptive,
    Linear,
}
impl Model {
    fn fit(self, samples: &[&Sample], prn: usize, cutoff: f64) -> Result<Clock> {
        match self {
            Self::Adaptive => fit(samples, prn, cutoff),
            Self::Linear => fit_model(samples, prn, cutoff, self),
        }
    }
}
fn evaluate(beta: &[f64], x: f64) -> f64 {
    beta.iter().rev().fold(0., |a, b| a * x + b)
}
pub fn fit(samples: &[&Sample], prn: usize, cutoff: f64) -> Result<Clock> {
    fit_model(samples, prn, cutoff, Model::Adaptive)
}
fn fit_model(samples: &[&Sample], prn: usize, cutoff: f64, model: Model) -> Result<Clock> {
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
    if matches!(model, Model::Linear) {
        ensure!(
            scores[0] <= CLOCK_RMS_LIMIT_NS * 1e-9,
            "Linear clock failed the held-out past-day prediction check"
        );
    }
    let degree = if matches!(model, Model::Adaptive)
        && scores[1] < 0.8 * scores[0]
        && scores[0] - scores[1] > 0.2e-9
    {
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
    select_model(train, rapid, prn, cutoff, Model::Adaptive)
}
fn select_model(
    train: &[&Sample],
    rapid: &[Sample],
    prn: usize,
    cutoff: f64,
    model: Model,
) -> Result<Clock> {
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
            model.fit(&samples, prn, t).ok().map(|mut c| {
                c.coefficients = shift(c.coefficients, cutoff - t);
                c
            })
        });
    if rapid_cutoff == Some(cutoff) {
        return fallback.context("No valid Rapid clock fit");
    }
    let candidate = model.fit(train, prn, cutoff);
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

/// Fit the TG-6's actual linear representation directly to observations. Its
/// causal hindcast fits three days ending 14 days ago and scores the following
/// full fortnight. No scored epoch is used by that historical fit, and no epoch
/// at or after today's forecast cutoff influences either fit or validation.
pub fn select_tg6(
    train: &[&Sample],
    rapid: &[Sample],
    history: &[Sample],
    prn: usize,
    cutoff: f64,
) -> Result<Tg6Clock> {
    let model = select_model(train, rapid, prn, cutoff, Model::Linear)?;
    let historical_cutoff = cutoff - TG6_FORECAST_DAYS as f64 * DAY;
    let historical: Vec<_> = history
        .iter()
        .filter(|s| s.time < historical_cutoff)
        .collect();
    let fit = fit_model(&historical, prn, historical_cutoff, Model::Linear)
        .context("Missing usable historical linear clock fit for the 14-day forecast check")?;
    let observed: Vec<_> = history
        .iter()
        .filter(|s| s.time >= historical_cutoff && s.time < cutoff)
        .filter_map(|s| s.clocks[prn - 1].map(|clock| (s.time, clock)))
        .collect();
    ensure!(
        observed.len() >= TG6_FORECAST_DAYS * 80
            && observed
                .first()
                .is_some_and(|s| s.0 <= historical_cutoff + 900.)
            && observed.windows(2).all(|w| w[1].0 - w[0].0 <= 3600.),
        "Incomplete observed clocks for the past 14-day forecast check"
    );
    let errors: Vec<_> = observed
        .iter()
        .map(|&(time, clock)| {
            (evaluate(&fit.coefficients, time - historical_cutoff - 10800.) - clock) * 1e9
        })
        .collect();
    let clock = Tg6Clock {
        model,
        validation: ForecastValidation {
            forecast_days: TG6_FORECAST_DAYS,
            forecast_start_gps_seconds: historical_cutoff,
            training_end_gps_seconds: fit.training_end_gps_seconds,
            observed_end_gps_seconds: observed.last().unwrap().0,
            samples: errors.len(),
            rms_ns: (errors.iter().map(|v| v * v).sum::<f64>() / errors.len() as f64).sqrt(),
            max_absolute_ns: errors.iter().map(|v| v.abs()).fold(0., f64::max),
        },
    };
    clock.validate(cutoff)?;
    Ok(clock)
}
pub fn shift([a, b, c]: [f64; 3], t: f64) -> [f64; 3] {
    [a + b * t + c * t * t, b + 2. * c * t, c]
}

#[cfg(test)]
#[path = "clock_audit_tests.rs"]
mod audit_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn curved_history(cutoff: f64, curvature: f64) -> Vec<Sample> {
        (0..20 * 96)
            .map(|i| {
                let time = cutoff - 20. * DAY + i as f64 * 900.;
                let elapsed = time - cutoff;
                Sample {
                    time,
                    positions: vec![Some([26000000., 0., 0.]); 32],
                    clocks: vec![
                        Some(1e-4 + elapsed * 1e-13 + curvature * (elapsed / DAY).powi(2));
                        32
                    ],
                }
            })
            .collect()
    }

    #[test]
    fn tg6_refits_a_valid_linear_model_even_when_tg1_selects_quadratic() {
        let cutoff = 30. * DAY;
        let history = curved_history(cutoff, 0.15e-9);
        let train: Vec<_> = history.iter().collect();
        let native = select(&train, &history, 1, cutoff).unwrap();
        assert_eq!(native.degree, 2);
        let tg6 = select_tg6(&train, &history, &history, 1, cutoff).unwrap();
        assert_eq!(tg6.model.degree, 1);
        assert_eq!(tg6.model.coefficients[2], 0.);
        assert!((tg6.model.coefficients[0] - native.coefficients[0]).abs() > 1e-12);
        assert_eq!(tg6.validation.samples, 14 * 96);
        assert_eq!(
            tg6.validation.training_end_gps_seconds,
            cutoff - 14. * DAY - 900.
        );
        assert!(tg6.validation.rms_ns < 50.);
        assert!(tg6.validation.max_absolute_ns < 150.);
    }

    #[test]
    fn tg6_checks_full_fortnight_error_and_requires_historical_observations() {
        let cutoff = 30. * DAY;
        let history = curved_history(cutoff, 5e-9);
        let train: Vec<_> = history.iter().collect();
        assert!(select(&train, &history, 1, cutoff).is_ok());
        let error = select_tg6(&train, &history, &history, 1, cutoff)
            .err()
            .unwrap();
        assert!(error.to_string().contains("past 14-day forecast check"));
        let recent = &history[17 * 96..];
        assert!(select_tg6(&train, &history, recent, 1, cutoff).is_err());
    }

    #[test]
    fn tg6_fit_and_fortnight_validation_ignore_all_future_epochs() {
        let cutoff = 30. * DAY;
        let mut history = curved_history(cutoff, 0.15e-9);
        let expected = select_tg6(
            &history.iter().collect::<Vec<_>>(),
            &history,
            &history,
            1,
            cutoff,
        )
        .unwrap();
        history.extend(curved_history(cutoff + 20. * DAY, 1.));
        let actual = select_tg6(
            &history.iter().collect::<Vec<_>>(),
            &history,
            &history,
            1,
            cutoff,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(expected).unwrap(),
            serde_json::to_value(actual).unwrap()
        );
    }
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
