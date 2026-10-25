// SPDX-License-Identifier: MIT
//! Optional research export using the production fitter. No network or camera.
use super::*;
use crate::{files, predictor::data};
use serde_json::json;
use std::{fs, path::PathBuf};

#[test]
#[ignore = "Requires an external observed IGS archive; exports offline research forecasts"]
fn export_historical_clock_forecasts() {
    let directory = PathBuf::from(
        std::env::var("TOUGHFIX_CLOCK_AUDIT_DATA").expect("Set TOUGHFIX_CLOCK_AUDIT_DATA"),
    );
    let output = PathBuf::from(
        std::env::var("TOUGHFIX_CLOCK_AUDIT_OUTPUT").expect("Set TOUGHFIX_CLOCK_AUDIT_OUTPUT"),
    );
    let observed = data::observations(&directory).unwrap();
    let samples = &observed.samples;
    let first = samples.first().unwrap().time;
    let end = samples.last().unwrap().time + 900.;
    assert_eq!(first % DAY, 0.);
    let mut cutoff = first + 17. * DAY;
    let mut rows = Vec::new();
    while cutoff + 14. * DAY <= end {
        // Pass physically separate past slices into the fitter. The caller
        // withholds all subsequent observations until after this export.
        let prior = &samples[..samples.partition_point(|s| s.time < cutoff)];
        let train: Vec<_> = prior
            .iter()
            .filter(|s| s.time >= cutoff - 3. * DAY)
            .collect();
        let historical_cutoff = cutoff - 14. * DAY;
        let historical: Vec<_> = samples[..samples.partition_point(|s| s.time < historical_cutoff)]
            .iter()
            .collect();
        for prn in 1..=32 {
            let current = select_model(&train, prior, prn, cutoff, Model::Linear);
            let historical = Model::Linear.fit(&historical, prn, historical_cutoff);
            if let Ok(c) = &current {
                assert!(c.training_end_gps_seconds < cutoff);
            }
            if let Ok(c) = &historical {
                assert!(c.training_end_gps_seconds < historical_cutoff);
            }
            // Reproduce the legacy fortnight gate used by the published study.
            let production_gate_passed = select_tg6(&train, prior, prior, prn, cutoff)
                .ok()
                .and_then(|c| c.validation)
                .is_some_and(|v| {
                    v.rms_ns <= CLOCK_RMS_LIMIT_NS && v.max_absolute_ns <= TG6_FORECAST_MAX_NS
                });
            rows.push(json!({"prn":prn,"cutoff_gps":cutoff,"historical_cutoff_gps":historical_cutoff,
                "production_gate_passed":production_gate_passed,
                "current":current.as_ref().ok(),"current_error":current.err().map(|e|format!("{e:#}")),
                "historical":historical.as_ref().ok(),"historical_error":historical.err().map(|e|format!("{e:#}"))}));
        }
        cutoff += DAY;
    }
    assert!(!rows.is_empty());
    let sources: Vec<_> = observed.paths.iter().map(|p| json!({
        "file":p.file_name().unwrap().to_string_lossy(),"sha256":crate::camera::hash(&fs::read(p).unwrap())
    })).collect();
    files::json(&output, &json!({"schema_version":1,"engine":"Production TG-6 linear clock fitter",
        "fitter_sha256":crate::camera::hash(include_bytes!("clocks.rs")),
        "observation_reader_sha256":crate::camera::hash(include_bytes!("data.rs")),
        "thresholds":{"rms_ns":CLOCK_RMS_LIMIT_NS,"maximum_ns":TG6_FORECAST_MAX_NS},
        "dataset_start_gps":first,"dataset_end_exclusive_gps":end,"sources":sources,"forecasts":rows})).unwrap();
}

#[test]
#[ignore = "Requires an external observed IGS archive; exports offline 28-day forecasts"]
fn export_tg6_four_week_clock_forecasts() {
    let directory = PathBuf::from(
        std::env::var("TOUGHFIX_CLOCK_AUDIT_DATA").expect("Set TOUGHFIX_CLOCK_AUDIT_DATA"),
    );
    let output = PathBuf::from(
        std::env::var("TOUGHFIX_CLOCK_AUDIT_OUTPUT").expect("Set TOUGHFIX_CLOCK_AUDIT_OUTPUT"),
    );
    let observed = data::observations(&directory).unwrap();
    let samples = &observed.samples;
    let first = samples.first().unwrap().time;
    let end = samples.last().unwrap().time + 900.;
    let mut cutoff = first + 3. * DAY;
    let mut rows = Vec::new();
    while cutoff + TG6_FORECAST_DAYS as f64 * DAY <= end {
        let prior = &samples[..samples.partition_point(|s| s.time < cutoff)];
        let train: Vec<_> = prior
            .iter()
            .filter(|s| s.time >= cutoff - 3. * DAY)
            .collect();
        for prn in 1..=32 {
            let selected = select_tg6(&train, prior, prior, prn, cutoff);
            if let Ok(clock) = &selected {
                assert!(clock.model.training_end_gps_seconds < cutoff);
            }
            rows.push(json!({"prn":prn,"cutoff_gps":cutoff,
                "current":selected.as_ref().ok().map(|c| &c.model),
                "current_error":selected.err().map(|e| format!("{e:#}"))}));
        }
        cutoff += DAY;
    }
    assert!(!rows.is_empty());
    let sources: Vec<_> = observed.paths.iter().map(|p| json!({
        "file":p.file_name().unwrap().to_string_lossy(),"sha256":crate::camera::hash(&fs::read(p).unwrap())
    })).collect();
    files::json(&output, &json!({"schema_version":2,"engine":"Production TG-6 four-week recent-observation clock policy",
        "forecast_days":TG6_FORECAST_DAYS,"fitter_sha256":crate::camera::hash(include_bytes!("clocks.rs")),
        "observation_reader_sha256":crate::camera::hash(include_bytes!("data.rs")),
        "dataset_start_gps":first,"dataset_end_exclusive_gps":end,"sources":sources,"forecasts":rows})).unwrap();
}
