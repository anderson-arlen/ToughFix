// SPDX-License-Identifier: MIT
//! Independent fixed-step RK4 check of saved DOP853 trajectories. No camera access.
use super::{
    data::{self, V3},
    physics::{Gravity, Model},
};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
struct Fitted {
    state: [f64; 6],
    forces: [f64; 5],
}
#[derive(Deserialize)]
struct Satellite {
    prn: usize,
    fit: Fitted,
    trajectory_6h_gcrs: Vec<[f64; 6]>,
}
#[derive(Deserialize)]
struct Diagnostics {
    forecast_start_gps: String,
    training_start_gps: String,
    satellites: Vec<Satellite>,
}
fn rhs(model: &Model, t: f64, y: [f64; 6], forces: [f64; 5]) -> [f64; 6] {
    let a = model.acceleration(
        t,
        V3::from_row_slice(&y[..3]),
        V3::from_row_slice(&y[3..]),
        forces,
    );
    [y[3], y[4], y[5], a.x, a.y, a.z]
}
fn rk4(model: &Model, s: &Satellite, step: f64) -> Result<Vec<[f64; 6]>> {
    let count = (17. * data::DAY / step) as usize;
    let sample = (21600. / step) as usize;
    ensure!(
        sample > 0 && count as f64 * step == 17. * data::DAY && sample as f64 * step == 21600.,
        "RK4 step must divide the grid"
    );
    let mut state = s.fit.state;
    let mut out = vec![state];
    for n in 0..count {
        let t = n as f64 * step;
        let a = rhs(model, t, state, s.fit.forces);
        let b = rhs(
            model,
            t + step / 2.,
            std::array::from_fn(|k| state[k] + a[k] * step / 2.),
            s.fit.forces,
        );
        let c = rhs(
            model,
            t + step / 2.,
            std::array::from_fn(|k| state[k] + b[k] * step / 2.),
            s.fit.forces,
        );
        let d = rhs(
            model,
            t + step,
            std::array::from_fn(|k| state[k] + c[k] * step),
            s.fit.forces,
        );
        for k in 0..6 {
            state[k] += step / 6. * (a[k] + 2. * b[k] + 2. * c[k] + d[k]);
        }
        if (n + 1) % sample == 0 {
            out.push(state);
        }
    }
    ensure!(out.len() == 69, "RK4 output grid mismatch");
    Ok(out.into_iter().skip(12).collect())
}
pub fn check(prefix: &Path, data_dir: &Path, selected: &[usize]) -> Result<Value> {
    let diagnostics: Diagnostics =
        serde_json::from_slice(&fs::read(prefix.with_file_name(format!(
            "{}-diagnostics.json",
            prefix
                .file_name()
                .context("Invalid prefix")?
                .to_string_lossy()
        )))?)?;
    let cutoff = data::epoch(&diagnostics.forecast_start_gps)?;
    let start = data::epoch(&diagnostics.training_start_gps)?;
    ensure!(
        cutoff - start == 3. * data::DAY,
        "Audit requires a three-day training arc"
    );
    ensure!(
        diagnostics.satellites.iter().all(|s| {
            s.fit
                .state
                .iter()
                .chain(s.fit.forces.iter())
                .all(|v| v.is_finite())
                && s.trajectory_6h_gcrs.len() == 57
                && s.trajectory_6h_gcrs.iter().flatten().all(|v| v.is_finite())
        }),
        "Invalid fitted state or saved trajectory"
    );
    let frames = data::Frames::read(&data_dir.join("finals2000A.all"))?;
    let model = Model::new(
        &frames,
        Gravity::read(&data_dir.join("egm96.zip"))?,
        start,
        cutoff + 14. * data::DAY,
    )?;
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build()?;
    let satellites: Vec<_> = diagnostics
        .satellites
        .iter()
        .filter(|s| selected.is_empty() || selected.contains(&s.prn))
        .collect();
    ensure!(!satellites.is_empty(), "No audit satellites selected");
    let rows:Vec<_>=pool.install(||satellites.par_iter().map(|s|->Result<_>{
        let coarse=rk4(&model,s,4.)?;let fine=rk4(&model,s,2.)?;
        ensure!(fine.len()==s.trajectory_6h_gcrs.len(),"Saved trajectory grid mismatch");
        let differences=|a:&[[f64;6]],b:&[[f64;6]]|a.iter().zip(b).map(|(a,b)|(V3::from_row_slice(&a[..3])-V3::from_row_slice(&b[..3])).norm()).collect::<Vec<_>>();
        let convergence=super::stats(&differences(&coarse,&fine));
        let agreement=super::stats(&differences(&fine,&s.trajectory_6h_gcrs));
        eprintln!("G{:02}: RK4 convergence max {:.4} m; DOP853/RK4 max {:.4} m",s.prn,convergence["max_m"].as_f64().unwrap(),agreement["max_m"].as_f64().unwrap());
        Ok(json!({"prn":s.prn,"rk4_step_4_vs_2_seconds":convergence,"dop853_vs_rk4_step_2_seconds":agreement}))
    }).collect());
    let rows: Vec<_> = rows.into_iter().collect::<Result<_>>()?;
    let allowed = rows.iter().all(|r| {
        ["rk4_step_4_vs_2_seconds", "dop853_vs_rk4_step_2_seconds"]
            .iter()
            .all(|k| {
                r[k]["max_m"].as_f64().unwrap() <= 0.5 && r[k]["rms_m"].as_f64().unwrap() <= 0.05
            })
    });
    let report = json!({"algorithm":"Classical fixed-step RK4, separate from DOP853","forecast_start_gps":diagnostics.forecast_start_gps,"satellites":rows,"passed":allowed});
    crate::files::json(
        &prefix.with_file_name(format!(
            "{}-rk4-audit.json",
            prefix.file_name().unwrap().to_string_lossy()
        )),
        &report,
    )?;
    ensure!(
        allowed,
        "Independent RK4 comparison exceeds 0.05 m RMS or 0.5 m maximum"
    );
    Ok(report)
}
pub fn cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    let root = super::engine::root(&crate::default_state_dir());
    let mut prefix = root.join("generation/native");
    let mut data_dir = root.join("data");
    let mut selected = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--prefix" => prefix = PathBuf::from(args.next().context("--prefix needs a path")?),
            "--data-dir" => {
                data_dir = PathBuf::from(args.next().context("--data-dir needs a path")?)
            }
            "--satellites" => {
                selected = args
                    .next()
                    .context("--satellites needs PRNs")?
                    .split(',')
                    .map(str::parse)
                    .collect::<std::result::Result<_, _>>()?
            }
            "--help" | "-h" => {
                println!(
                    "toughfix audit [--prefix PATH] [--data-dir PATH] [--satellites 1,2,...]\nIndependent fixed-step RK4 check of saved DOP853 trajectories. Never accesses a camera."
                );
                return Ok(());
            }
            _ => anyhow::bail!("Unknown audit option {arg}"),
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&check(&prefix, &data_dir, &selected)?)?
    );
    Ok(())
}
