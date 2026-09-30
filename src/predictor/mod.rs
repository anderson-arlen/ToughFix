// SPDX-License-Identifier: MIT
pub mod audit;
mod cep;
mod clocks;
mod data;
pub mod engine;
pub mod health;
mod physics;
mod sources;
pub mod validation;

use anyhow::{Context, Result, bail, ensure};
use data::{DAY, Frames, V3};
use rayon::prelude::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, time::Instant};

pub struct Options {
    pub data_dir: PathBuf,
    pub output_prefix: PathBuf,
    pub cutoff: Option<f64>,
    pub satellites: Vec<usize>,
    pub threads: usize,
    pub include_ultra: bool,
}
struct Satellite {
    prn: usize,
    fitted: physics::Fitted,
    clock: clocks::Clock,
    states: Vec<[f64; 6]>,
    fixed: Vec<V3>,
    records: Vec<Vec<u8>>,
    weeks: Vec<Value>,
    errors: Vec<f64>,
    convergence: Value,
}
fn stats(values: &[f64]) -> Value {
    if values.is_empty() {
        return json!({"count":0});
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |q: f64| {
        let x = q * (sorted.len() - 1) as f64;
        let k = x.floor() as usize;
        sorted[k] + (x - k as f64) * (sorted[x.ceil() as usize] - sorted[k])
    };
    json!({"count":values.len(),"rms_m":(values.iter().map(|x|x*x).sum::<f64>()/values.len() as f64).sqrt(),"median_m":percentile(0.5),"p95_m":percentile(0.95),"max_m":sorted.last().unwrap()})
}
fn suffix(prefix: &std::path::Path, tail: &str) -> PathBuf {
    let mut p = prefix.as_os_str().to_os_string();
    p.push(tail);
    p.into()
}

struct Prediction<'a> {
    model: &'a physics::Model,
    frames: &'a Frames,
    train: &'a [&'a data::Sample],
    rotations: &'a [data::M3],
    future_frames: &'a [data::M3],
    cutoff: f64,
    rapid: &'a [data::Sample],
}
impl Prediction<'_> {
    fn satellite(&self, prn: usize) -> Result<Satellite> {
        let model = self.model;
        let frames = self.frames;
        let train = self.train;
        let rotations = self.rotations;
        let future_frames = self.future_frames;
        let cutoff = self.cutoff;

        let positions: Vec<_> = train
            .iter()
            .zip(rotations)
            .map(|(s, r)| {
                s.positions[prn - 1]
                    .map(|p| r.transpose() * V3::from(p))
                    .context("Missing observed orbital sample")
            })
            .collect::<Result<_>>()?;
        let fitted = physics::fit(model, &positions, 3)?;
        ensure!(
            fitted.rms_m <= 5.,
            "Observed orbit fit RMS {:.3} m exceeds 5 m",
            fitted.rms_m
        );
        let clock = clocks::select(train, self.rapid, prn, cutoff)?;
        let trajectory = model.propagate(&[fitted.state], &[fitted.forces], 17. * DAY, 300.)?;
        let refined =
            model.propagate_with_step(&[fitted.state], &[fitted.forces], 17. * DAY, 300., 30.)?;
        let convergence_errors: Vec<_> = trajectory[864..]
            .iter()
            .zip(&refined[864..])
            .map(|(a, b)| (V3::from_row_slice(&a[0][..3]) - V3::from_row_slice(&b[0][..3])).norm())
            .collect();
        let convergence = stats(&convergence_errors);
        ensure!(
            convergence["rms_m"].as_f64().unwrap() <= 0.05
                && convergence["max_m"].as_f64().unwrap() <= 0.5,
            "Eclipse/integration convergence exceeds 0.05 m RMS or 0.5 m maximum"
        );
        let states: Vec<_> = refined[864..].iter().map(|s| s[0]).collect();
        ensure!(states.len() == 4033, "Forecast grid mismatch");
        let fixed: Vec<_> = states
            .iter()
            .zip(future_frames)
            .map(|(s, r)| r * V3::from_row_slice(&s[..3]))
            .collect();
        let mut records = Vec::new();
        let mut weeks = Vec::new();
        let mut errors = Vec::new();
        for week in 0..2 {
            let attempt = (|| -> Result<(Vec<u8>, Vec<f64>)> {
                let mut parameters = Vec::new();
                let mut local = Vec::new();
                for slot in 0..28 {
                    let arc = week * 28 + slot;
                    let reference = cutoff + arc as f64 * 21600. + 10800.;
                    let center = arc * 72 + 36;
                    let state = states[center];
                    let rotation = cep::pseudo(reference, future_frames[center]);
                    let derivative = (cep::pseudo(reference + 1., frames.matrix(reference + 1.)?)
                        - cep::pseudo(reference - 1., frames.matrix(reference - 1.)?))
                        / 2.;
                    let r = V3::from_row_slice(&state[..3]);
                    let v = V3::from_row_slice(&state[3..]);
                    let initial = cep::kepler(rotation * r, rotation * v + derivative * r);
                    let times: Vec<_> = (0..=24)
                        .map(|j| cutoff + (arc * 72 + j * 3) as f64 * 300.)
                        .collect();
                    let targets: Vec<_> = (0..=24).map(|j| fixed[arc * 72 + j * 3]).collect();
                    let p = cep::fit(initial, reference, &times, &targets)?;
                    for j in 0..=72 {
                        let index = arc * 72 + j;
                        local.push(
                            (cep::position(&p, reference, cutoff + index as f64 * 300.)
                                .context("Nonphysical broadcast fit")?
                                - fixed[index])
                                .norm(),
                        );
                    }
                    parameters.push(p);
                }
                ensure!(
                    local.iter().all(|e| *e <= 15.),
                    "Broadcast approximation exceeds 15 metres"
                );
                let record = cep::encode(
                    prn as u8,
                    &parameters,
                    clocks::shift(clock.coefficients, week as f64 * 604800.),
                )?;
                // Check quantized output with the independent receiver-format reader.
                let mut decoded_errors = Vec::new();
                for slot in 0..28 {
                    let p = validation::radians(&record, slot)?;
                    let arc = week * 28 + slot;
                    let reference = cutoff + arc as f64 * 21600. + 10800.;
                    for j in 0..=72 {
                        let at = arc * 72 + j;
                        decoded_errors.push(
                            (cep::position(&p, reference, cutoff + at as f64 * 300.)
                                .context("Nonphysical quantized broadcast fit")?
                                - fixed[at])
                                .norm(),
                        );
                    }
                }
                ensure!(
                    decoded_errors.iter().all(|e| *e <= 15.),
                    "Quantized broadcast approximation exceeds 15 metres"
                );
                Ok((record, decoded_errors))
            })();
            match attempt {
                Ok((record, local)) => {
                    weeks.push(json!({"week":week+1,"prn":prn,"available":true,"broadcast_approximation":stats(&local)}));
                    records.push(record);
                    errors.extend(local);
                }
                Err(e) => {
                    weeks.push(json!({"week":week+1,"prn":prn,"available":false,"reason":format!("{e:#}")}));
                    records.push(cep::unavailable());
                }
            }
        }
        Ok(Satellite {
            prn,
            fitted,
            clock,
            states,
            fixed,
            records,
            weeks,
            errors,
            convergence,
        })
    }
}

pub fn generate(options: &Options, progress: impl Fn(String) + Sync) -> Result<Value> {
    let began = Instant::now();
    ensure!(
        options.threads > 0 && options.threads <= 32,
        "Use 1–32 prediction workers"
    );
    let rapid = data::observations(&options.data_dir)?;
    let observed = data::observations_with_ultra(&options.data_dir, options.include_ultra)?;
    let cutoff = match options.cutoff {
        Some(t) => t,
        None => data::latest_cutoff(&observed, health::now()?)?,
    };
    let start = cutoff - 3. * DAY;
    ensure!(
        cutoff.is_finite()
            && cutoff.fract() == 0.
            && start >= 0.
            && cutoff + 3. * 604800. < u32::MAX as f64,
        "Cutoff is outside the CEP epoch range"
    );
    let train: Vec<_> = observed
        .samples
        .iter()
        .filter(|s| s.time >= start && s.time < cutoff)
        .collect();
    ensure!(
        train.len() == 288
            && train
                .iter()
                .enumerate()
                .all(|(i, s)| s.time == start + i as f64 * 900.),
        "Need a complete three-day observed arc on a 15-minute grid before {}",
        data::calendar(cutoff)
    );
    let frames = Frames::read(&options.data_dir.join("finals2000A.all"))?;
    let model = physics::Model::new(
        &frames,
        physics::Gravity::read(&options.data_dir.join("egm96.zip"))?,
        start,
        cutoff + 14. * DAY,
    )?;
    let rotations: Vec<_> = train
        .iter()
        .map(|s| frames.matrix(s.time))
        .collect::<Result<_>>()?;
    let future_frames: Vec<_> = (0..=4032)
        .map(|i| frames.matrix(cutoff + i as f64 * 300.))
        .collect::<Result<_>>()?;
    let wanted = if options.satellites.is_empty() {
        (1..=32).collect()
    } else {
        options.satellites.clone()
    };
    ensure!(
        wanted.iter().all(|s| (1..=32).contains(s)),
        "PRNs must be 1–32"
    );
    ensure!(
        wanted
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == wanted.len(),
        "Duplicate PRNs are not allowed"
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.threads)
        .build()?;
    progress(format!(
        "Fitting {} satellites from {} through {} GPS",
        wanted.len(),
        data::calendar(start),
        data::calendar(cutoff - 900.)
    ));
    let prediction = Prediction {
        model: &model,
        frames: &frames,
        train: &train,
        rotations: &rotations,
        future_frames: &future_frames,
        cutoff,
        rapid: &rapid.samples,
    };
    let results: Vec<_> = pool.install(|| {
        wanted
            .par_iter()
            .map(|&prn| {
                let result = prediction.satellite(prn);
                let message = match &result {
                    Ok(s) => format!(
                        "orbit RMS {:.3} m; {} usable weeks",
                        s.fitted.rms_m,
                        s.records.iter().filter(|r| r[1] == 0).count()
                    ),
                    Err(e) => format!("disabled: {e:#}"),
                };
                progress(format!("G{prn:02}: {message}"));
                (prn, result)
            })
            .collect()
    });
    let mut weeks = vec![vec![cep::unavailable(); 32]; 4];
    let mut satellites = Vec::new();
    let mut flags = Vec::new();
    let mut errors = Vec::new();
    let mut rows = Vec::new();
    let mut diagnostics = Vec::new();
    let mut clock_sources = std::collections::BTreeMap::<String, usize>::new();
    let mut health_start = start;
    for (prn, result) in results {
        match result {
            Ok(s) => {
                *clock_sources.entry(s.clock.source.clone()).or_default() += 1;
                health_start = health_start.min(s.clock.training_start_gps_seconds);
                for (week, r) in s.records.into_iter().enumerate() {
                    weeks[week][prn - 1] = r;
                }
                rows.extend(s.weeks);
                errors.extend(s.errors);
                let acceleration = model.acceleration(
                    0.,
                    V3::from_row_slice(&s.fitted.state[..3]),
                    V3::from_row_slice(&s.fitted.state[3..]),
                    s.fitted.forces,
                );
                let probes: Vec<_> = [0., 123.456, 1987.65, 86400.25, 259200.125, 1468799.9]
                    .iter()
                    .map(|&t| {
                        let a = model.acceleration(
                            t,
                            V3::from_row_slice(&s.fitted.state[..3]),
                            V3::from_row_slice(&s.fitted.state[3..]),
                            s.fitted.forces,
                        );
                        json!({"elapsed":t,"acceleration":a.as_slice()})
                    })
                    .collect();
                diagnostics.push(json!({"prn":s.prn,"fit":s.fitted,"clock":s.clock,"integration_convergence":s.convergence,"initial_acceleration_gcrs_mps2":acceleration.as_slice(),"acceleration_probes":probes,"trajectory_6h_gcrs":s.states.iter().step_by(72).collect::<Vec<_>>(),"trajectory_300s_itrf":s.fixed.iter().map(|v|v.as_slice()).collect::<Vec<_>>()}));
                satellites.push(prn);
            }
            Err(e) => flags.push(json!({"prn":prn,"reason":format!("{e:#}")})),
        }
    }
    ensure!(
        weeks[..2]
            .iter()
            .all(|week| week.iter().filter(|r| r[1] == 0).count() >= 4),
        "Fewer than four usable satellites in a prediction week; refusing empty assistance"
    );
    let archive = cep::archive(cutoff, &weeks)?;
    let hash = format!("{:x}", Sha256::digest(&archive));
    let sources: Vec<_> = observed.paths.iter().filter(|p| {
        data::file_interval(p).is_ok_and(|(a, b)| a < cutoff && b > health_start)
    }).map(|p| Ok(json!({"file":p.file_name().unwrap().to_string_lossy(),"sha256":format!("{:x}",Sha256::digest(fs::read(p)?))}))).collect::<Result<_>>()?;
    let report = json!({"integration":{"relative_tolerance":2e-13,"absolute_tolerance":1e-10,"fit_max_step_seconds":60,"forecast_max_step_seconds":30,"convergence_max_step_seconds":60,"convergence_max_m":0.5,"convergence_rms_m":0.05},"engine":"native Rust / ERFA / DOP853 / EGM96 degree 8 / Sun-Moon / ECOM5","forecast_start_gps":data::calendar(cutoff),"start_gps":data::calendar(cutoff),"validity_end_exclusive_gps":data::calendar(cutoff+14.*DAY),"training_start_gps":data::calendar(model.start),"training_end_gps":data::calendar(cutoff-900.),"latest_fitted_observed_gps_seconds":cutoff-900.,"health_training_start_gps":data::calendar(health_start),"clock_sources":clock_sources,"clock_policy":"Newer observed clock residual RMS <= max(1 ns, 2x Rapid); residual step <= max(5 ns, 4x Rapid); otherwise Rapid <= 48 hours old, reanchored to orbit cutoff","forecast_days":14,"bytes":archive.len(),"sha256":hash,"available_prns_by_week":weeks.iter().map(|w|w.iter().enumerate().filter(|(_,r)|r[1]==0).map(|(i,_)|i+1).collect::<Vec<_>>()).collect::<Vec<_>>(),"quality_flags":flags,"records":rows,"broadcast_approximation":stats(&errors),"source_data":if options.include_ultra {"IGS Rapid + observed Ultra-rapid · NOAA public archive"} else {"IGS Rapid observations · NOAA public archive"},"source_files":sources,"latest_observed_orbit_gps":data::calendar(cutoff-900.),"earth_orientation_sha256":format!("{:x}",Sha256::digest(fs::read(options.data_dir.join("finals2000A.all"))?)),"gravity_sha256":format!("{:x}",Sha256::digest(fs::read(options.data_dir.join("egm96.zip"))?)),"fit":{"fit_3d_rms_m":(diagnostics.iter().filter_map(|s|s["fit"]["rms_m"].as_f64()).map(|r|r*r).sum::<f64>()/diagnostics.len().max(1) as f64).sqrt()},"total_seconds":began.elapsed().as_secs_f64(),"upload_allowed":false,"model_warning":"Experimental native candidate. Live NANU/observed-event health guarding and independent CEP validation required before upload.","archive_tail":"weeks 3/4 explicitly unavailable"});
    fs::create_dir_all(
        options
            .output_prefix
            .parent()
            .context("Output prefix needs a directory")?,
    )?;
    fs::write(
        suffix(&options.output_prefix, "-diagnostics.json"),
        serde_json::to_vec(
            &json!({"forecast_start_gps":data::calendar(cutoff),"training_start_gps":data::calendar(start),"satellites":diagnostics}),
        )?,
    )?;
    fs::write(suffix(&options.output_prefix, "-generated.cep"), archive)?;
    fs::write(
        suffix(&options.output_prefix, "-metrics.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}
pub fn cli(args: impl Iterator<Item = String>) -> Result<()> {
    let root = engine::root(&crate::default_state_dir());
    let mut options = Options {
        data_dir: root.join("data"),
        output_prefix: root.join("offline-generation/native"),
        cutoff: None,
        satellites: Vec::new(),
        threads: 4,
        include_ultra: true,
    };
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                options.data_dir = args.next().context("--data-dir needs a path")?.into()
            }
            "--output-prefix" => {
                options.output_prefix = args.next().context("--output-prefix needs a path")?.into()
            }
            "--forecast-start" => {
                options.cutoff = Some(data::epoch(
                    &args
                        .next()
                        .context("--forecast-start needs a GPS timestamp")?,
                )?)
            }
            "--satellites" => {
                options.satellites = args
                    .next()
                    .context("--satellites needs comma-separated PRNs")?
                    .split(',')
                    .map(str::parse)
                    .collect::<std::result::Result<_, _>>()?
            }
            "--rapid-only" => options.include_ultra = false,
            "--threads" => {
                options.threads = args.next().context("--threads needs a number")?.parse()?
            }
            "--help" | "-h" => {
                println!(
                    "toughfix predict [--data-dir PATH] [--output-prefix PATH] [--forecast-start YYYY-MM-DDTHH:MM:SS] [--satellites 1,2,...] [--threads 4] [--rapid-only]\nOffline native generation from observed IGS Rapid/Ultra-rapid SP3, EGM96, and USNO EOP. Default cutoff: latest observed epoch + 15 minutes. Produces a separate experimental candidate; never uploads."
                );
                return Ok(());
            }
            _ => bail!("Unknown predictor option {arg}"),
        }
    }
    let report = generate(&options, |s| eprintln!("{s}"))?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
