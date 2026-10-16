// SPDX-License-Identifier: MIT
//! Transactional desktop pipeline: official sources → fit → decode → guard → publish.
use super::{
    Options, Progress, RefreshStage, data, generate,
    health::{self, Snapshot},
    sources::Sources,
    tg6, validation,
};
use crate::{files, model::CameraModel};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

pub fn root(state: &Path) -> PathBuf {
    state.join("engine")
}
pub fn current(state: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(
        root(state).join("current.json"),
    )?)?)
}
pub fn current_for(state: &Path, model: CameraModel) -> Result<Value> {
    let pointer = current(state)?;
    if model == CameraModel::Tg6 {
        ensure!(
            pointer["tg6"].is_object(),
            "TG-6 predictions need a successful refresh"
        );
        Ok(pointer["tg6"].clone())
    } else {
        Ok(pointer)
    }
}
pub fn archive(state: &Path, pointer: &Value) -> Result<Vec<u8>> {
    let name = pointer["archive"]
        .as_str()
        .context("Missing published archive")?;
    ensure!(
        name.len() == 68
            && name.ends_with(".cep")
            && name.as_bytes()[..64].iter().all(u8::is_ascii_hexdigit),
        "Invalid published archive filename"
    );
    let bytes = fs::read(root(state).join("archives").join(name))?;
    ensure!(
        crate::camera::hash(&bytes) == name[..64],
        "Published archive hash mismatch"
    );
    Ok(bytes)
}
fn lock(path: &Path) -> Result<File> {
    fs::create_dir_all(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.join("engine.lock"))?;
    // SAFETY: valid descriptor owned by file; released automatically on drop.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another prediction refresh is already running"
    );
    Ok(file)
}
pub fn refresh(state: &Path, offline: bool, progress: impl Fn(Progress) + Sync) -> Result<Value> {
    let root = root(state);
    let _lock = lock(&root)?;
    let mut next_refresh_gps = None;
    let result = (|| {
        let cache = root.join("data");
        let snapshot: Snapshot = if offline {
            serde_json::from_slice(&fs::read(cache.join("health-state.json"))?)?
        } else {
            let mut sources = Sources::new(&cache)?;
            let result = sources.refresh(&progress);
            next_refresh_gps = sources.next_refresh_at(result.is_ok());
            result?
        };
        publish(state, &snapshot, health::now()?, &progress)
    })();
    files::json(
        &root.join("status.json"),
        &match &result {
            Ok(_) => {
                json!({"refresh_ok":true,"checked_at_gps":health::now()?,"next_refresh_gps":next_refresh_gps})
            }
            Err(e) => {
                json!({"refresh_ok":false,"checked_at_gps":health::now()?,"error":format!("{e:#}"),"next_refresh_gps":next_refresh_gps})
            }
        },
    )?;
    result
}

pub fn source_refresh_wait(state: &Path, now: f64) -> std::time::Duration {
    let next = fs::read(root(state).join("status.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["next_refresh_gps"].as_f64());
    next.filter(|t| (0. ..=86400.).contains(&(t - now)))
        .map_or(std::time::Duration::ZERO, |t| {
            std::time::Duration::from_secs_f64(t - now)
        })
}

fn fingerprint(observed: &data::Observations, cache: &Path, cutoff: f64) -> Result<String> {
    let selected: Vec<_> = observed
        .samples
        .iter()
        .filter(|s| s.time >= cutoff - 3. * data::DAY && s.time < cutoff)
        .collect();
    let rapid = data::observations(cache)?;
    let rapid_cutoff = rapid
        .samples
        .iter()
        .rfind(|s| s.time < cutoff)
        .map_or(cutoff, |s| s.time + 900.);
    let clocks: Vec<_> = rapid
        .samples
        .iter()
        .filter(|s| s.time >= rapid_cutoff - 3. * data::DAY && s.time < rapid_cutoff)
        .collect();
    let mut bytes = serde_json::to_vec(&(selected, clocks))?;
    let validation_clocks: Vec<_> = observed
        .samples
        .iter()
        .filter(|s| s.time >= cutoff - 17. * data::DAY && s.time < cutoff)
        .map(|s| (s.time, &s.clocks))
        .collect();
    bytes.extend(serde_json::to_vec(&validation_clocks)?);
    bytes.extend(fs::read(cache.join("egm96.zip"))?);
    bytes.extend(fs::read(cache.join("finals2000A.all"))?);
    // Bump this whenever fit/encoding policy changes to invalidate old cached fits.
    bytes.extend_from_slice(b"toughfix-native-engine-v5-tg6-validated-linear-clocks");
    Ok(crate::camera::hash(&bytes))
}

pub(super) fn publish(
    state: &Path,
    snapshot: &Snapshot,
    now: f64,
    progress: &(impl Fn(Progress) + Sync),
) -> Result<Value> {
    let failures = snapshot.failures(now);
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    let root = root(state);
    let cache = root.join("data");
    let observed = data::observations_with_ultra(&cache, true)?;
    let cutoff = data::latest_cutoff(&observed, now)?;
    let fingerprint = fingerprint(&observed, &cache, cutoff)?;
    let old = current(state).ok();
    let reusable = old.as_ref().filter(|p| {
        p["input_fingerprint"].as_str() == Some(&fingerprint) && p["engine_version"] == 1
    });
    let (bytes, metrics) = if let Some(p) = reusable {
        progress(Progress::new(
            RefreshStage::Validating,
            "No newer orbital inputs; checking existing predictions",
        ));
        (archive(state, p)?, p["metrics"].clone())
    } else {
        let scratch = root.join("generation");
        fs::create_dir_all(&scratch)?;
        let metrics = generate(
            &Options {
                data_dir: cache.clone(),
                output_prefix: scratch.join("native"),
                cutoff: Some(cutoff),
                satellites: Vec::new(),
                threads: 4,
                include_ultra: true,
            },
            progress,
        )?;
        let bytes = fs::read(scratch.join("native-generated.cep"))?;
        ensure!(
            crate::camera::hash(&bytes)
                == metrics["sha256"]
                    .as_str()
                    .context("Missing generation hash")?,
            "Generation hash mismatch"
        );
        (bytes, metrics)
    };
    let training = data::epoch(
        metrics["health_training_start_gps"]
            .as_str()
            .or_else(|| metrics["training_start_gps"].as_str())
            .context("Missing training epoch")?,
    )?;
    progress(Progress::new(
        RefreshStage::Validating,
        "Checking satellite safety and the assistance file",
    ));
    let (guarded, mut guard) = health::guard(&bytes, training, snapshot, now)?;
    let validation = validation::validate(&guarded)?;
    let hash = crate::camera::hash(&guarded);
    guard["output_sha256"] = json!(hash);
    guard["validation"] = validation;
    let archive_name = format!("{hash}.cep");
    let path = root.join("archives").join(&archive_name);
    if !path.exists() {
        files::atomic(&path, &guarded)?;
    } else {
        ensure!(
            fs::read(&path)? == guarded,
            "Immutable published archive was altered"
        );
    }
    let mut pointer = json!({"engine_version":1,"archive":archive_name,"input_fingerprint":fingerprint,"metrics":metrics,"guard":guard});
    let tg6_result = (|| -> Result<Value> {
        let meta = &pointer["metrics"]["tg6"];
        ensure!(
            meta.is_object(),
            "{}",
            pointer["metrics"]["tg6_error"]
                .as_str()
                .unwrap_or("TG-6 generation is unavailable")
        );
        let bytes = if let Some(p) = reusable {
            archive(state, &p["tg6"])?
        } else {
            let bytes = fs::read(root.join("generation/native-tg6.bin"))?;
            ensure!(
                meta["sha256"].as_str() == Some(crate::camera::hash(&bytes).as_str()),
                "TG-6 generation hash mismatch"
            );
            bytes
        };
        let tg6_training = data::epoch(
            meta["health_training_start_gps"]
                .as_str()
                .context("Missing TG-6 clock/orbit training epoch")?,
        )?;
        let (tg6_base, tg6_health) = health::guard(&guarded, tg6_training, snapshot, now)?;
        let (bytes, guard) = tg6::guard(&bytes, &tg6_base, &tg6_health)?;
        let hash = crate::camera::hash(&bytes);
        let archive_name = format!("{hash}.cep");
        let path = root.join("archives").join(&archive_name);
        if path.exists() {
            ensure!(
                fs::read(path)? == bytes,
                "Immutable TG-6 archive was altered"
            );
        } else {
            files::atomic(&path, &bytes)?;
        }
        Ok(
            json!({"engine_version":1,"archive":archive_name,"metrics":meta,"guard":guard,
                  "base_sha256":pointer["guard"]["output_sha256"]}),
        )
    })();
    match tg6_result {
        Ok(value) => pointer["tg6"] = value,
        Err(e) => pointer["tg6_error"] = json!(format!("{e:#}")),
    }
    // The single atomic pointer publishes the bytes and both reports together.
    files::json(&root.join("current.json"), &pointer)?;
    progress(Progress::new(
        RefreshStage::Validating,
        if pointer["guard"]["upload_allowed"] == true {
            "Validated predictions are ready"
        } else {
            "Predictions published, but satellite health blocks uploading"
        },
    ));
    Ok(pointer)
}

pub fn preflight(state: &Path) -> Result<(Vec<u8>, Value)> {
    let root = root(state);
    let status: Value = serde_json::from_slice(&fs::read(root.join("status.json"))?)?;
    ensure!(
        status["refresh_ok"] == true,
        "Latest prediction refresh failed; retry before uploading"
    );
    let pointer = current(state)?;
    ensure!(
        pointer["engine_version"] == 1 && pointer["guard"]["upload_allowed"] == true,
        "Published prediction is not eligible for upload"
    );
    let bytes = archive(state, &pointer)?;
    let snapshot: Snapshot =
        serde_json::from_slice(&fs::read(root.join("data/health-state.json"))?)?;
    let now = health::now()?;
    ensure!(
        (0. ..=3600.).contains(&(now - snapshot.checked_at_gps)),
        "Satellite health needs refreshing before upload"
    );
    let training = data::epoch(
        pointer["metrics"]["health_training_start_gps"]
            .as_str()
            .or_else(|| pointer["metrics"]["training_start_gps"].as_str())
            .context("Missing fit epoch")?,
    )?;
    let (guarded, report) = health::guard(&bytes, training, &snapshot, now)?;
    ensure!(
        report["upload_allowed"] == true && guarded == bytes,
        "Prediction health/validity changed; refresh before uploading"
    );
    ensure!(
        pointer["guard"]["output_sha256"].as_str() == Some(crate::camera::hash(&bytes).as_str()),
        "Prediction report hash mismatch"
    );
    validation::validate(&bytes)?;
    Ok((bytes, pointer))
}
pub fn preflight_for(state: &Path, model: CameraModel) -> Result<(Vec<u8>, Value)> {
    let (base, pointer) = preflight(state)?;
    if model != CameraModel::Tg6 {
        return Ok((base, pointer));
    }
    let p = &pointer["tg6"];
    ensure!(
        p["engine_version"] == 1
            && p["guard"]["upload_allowed"] == true
            && p["base_sha256"] == pointer["guard"]["output_sha256"],
        "TG-6 prediction is not eligible for upload"
    );
    let bytes = archive(state, p)?;
    ensure!(
        bytes.len() == tg6::CAPACITY,
        "TG-6 prediction has the wrong transfer size"
    );
    let training = data::epoch(
        p["metrics"]["health_training_start_gps"]
            .as_str()
            .or_else(|| pointer["metrics"]["health_training_start_gps"].as_str())
            .or_else(|| pointer["metrics"]["training_start_gps"].as_str())
            .context("Missing TG-6 training epoch")?,
    )?;
    let snapshot: Snapshot =
        serde_json::from_slice(&fs::read(root(state).join("data/health-state.json"))?)?;
    let (base, base_health) = health::guard(&base, training, &snapshot, health::now()?)?;
    let (guarded, report) = tg6::guard(&bytes, &base, &base_health)?;
    ensure!(
        guarded == bytes
            && report["upload_allowed"] == true
            && p["guard"]["output_sha256"].as_str() == Some(crate::camera::hash(&bytes).as_str()),
        "TG-6 satellite health or file changed; refresh before uploading"
    );
    Ok((bytes, p.clone()))
}

pub fn cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    let mut state = crate::default_state_dir();
    let mut offline = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state-dir" => state = args.next().context("--state-dir needs a path")?.into(),
            "--offline" => offline = true,
            "--help" | "-h" => {
                println!(
                    "toughfix refresh [--state-dir PATH] [--offline]\nDownload official inputs, generate, validate and publish assistance. Never accesses a camera. Offline mode uses cached inputs and still enforces freshness."
                );
                return Ok(());
            }
            _ => anyhow::bail!("Unknown refresh option {arg}"),
        }
    }
    let result = refresh(&state, offline, |s| eprintln!("{s}"))?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        predictor::{cep, health::Event},
        test_support::Temp,
    };
    #[test]
    fn source_schedule_honours_persisted_directory_and_refusal_deadlines() {
        let temp = Temp::new();
        let now = health::now().unwrap();
        for (deadline, expected) in [
            (Some(now + 3600.), 3600.),
            (Some(now + 7200.), 7200.),
            (Some(now - 1.), 0.),
            (None, 0.),
        ] {
            files::json(
                &root(temp.path()).join("status.json"),
                &json!({"next_refresh_gps":deadline}),
            )
            .unwrap();
            assert_eq!(
                source_refresh_wait(temp.path(), now).as_secs_f64(),
                expected
            );
        }
    }
    fn seed(state: &Path) -> (Vec<u8>, Snapshot) {
        let now = health::now().unwrap();
        let start = (now / 900.).floor() * 900. - 86400.;
        let fixture = include_bytes!("../../tests/fixtures/working-camera.cep");
        let weeks: Vec<_> = (0..4)
            .map(|w| {
                (0..32)
                    .map(|p| {
                        let pos = w * validation::BLOCK + 6 + p * validation::RECORD;
                        fixture[pos..pos + validation::RECORD].to_vec()
                    })
                    .collect()
            })
            .collect();
        let bytes = cep::archive(start, &weeks).unwrap();
        let hash = crate::camera::hash(&bytes);
        files::atomic(&root(state).join(format!("archives/{hash}.cep")), &bytes).unwrap();
        let snapshot = Snapshot {
            schema_version: 1,
            refresh_ok: true,
            checked_at_gps: now,
            latest_observed_gps: now,
            latest_observed_by_prn: (1..=32).map(|p| (p.to_string(), now)).collect(),
            notices: Vec::new(),
            orbit_events: Vec::new(),
            refresh_error: None,
            latest_nanu: String::new(),
        };
        files::json(&root(state).join("data/health-state.json"), &snapshot).unwrap();
        files::json(
            &root(state).join("status.json"),
            &json!({"refresh_ok":true}),
        )
        .unwrap();
        files::json(&root(state).join("current.json"),&json!({"engine_version":1,"archive":format!("{hash}.cep"),"metrics":{"training_start_gps":data::calendar(start-3.*data::DAY)},"guard":{"output_sha256":hash,"upload_allowed":true}})).unwrap();
        (bytes, snapshot)
    }
    #[test]
    fn preflight_rechecks_health_and_never_trusts_stale_approval() {
        let temp = Temp::new();
        let (bytes, mut snapshot) = seed(temp.path());
        assert_eq!(preflight(temp.path()).unwrap().0, bytes);
        snapshot.orbit_events.push(Event {
            prn: 1,
            first_gps: snapshot.checked_at_gps - 100.,
            last_gps: snapshot.checked_at_gps - 50.,
            peak_innovation_m: 500.,
            samples: 1,
            reason: "disruption".into(),
        });
        files::json(&root(temp.path()).join("data/health-state.json"), &snapshot).unwrap();
        assert!(preflight(temp.path()).is_err());
        snapshot.orbit_events.clear();
        snapshot.checked_at_gps -= 3601.;
        files::json(&root(temp.path()).join("data/health-state.json"), &snapshot).unwrap();
        assert!(preflight(temp.path()).is_err());
        let (_, snapshot) = seed(temp.path());
        files::json(
            &root(temp.path()).join("status.json"),
            &json!({"refresh_ok":false}),
        )
        .unwrap();
        assert!(preflight(temp.path()).is_err());
        files::json(
            &root(temp.path()).join("status.json"),
            &json!({"refresh_ok":true}),
        )
        .unwrap();
        let mut pointer = current(temp.path()).unwrap();
        pointer["guard"]["output_sha256"] = json!("wrong");
        files::json(&root(temp.path()).join("current.json"), &pointer).unwrap();
        assert!(preflight(temp.path()).is_err());
        assert!(snapshot.refresh_ok);
    }

    #[test]
    fn tg6_preflight_is_model_specific_and_bound_to_current_guarded_native_file() {
        let temp = Temp::new();
        let (base, mut snapshot) = seed(temp.path());
        let start = validation::start(&base).unwrap();
        let mut a =
            tg6::Archive::decode(include_bytes!("../../tests/fixtures/tg6-state-vectors.bin"))
                .unwrap();
        let mut r = a.records[0].clone();
        r.state_epoch = (start / 900. + 44244. * 96.) as u32;
        r.clock_epoch = r.state_epoch;
        r.states = vec![r.states[0]; 168];
        a.records = (1..=6)
            .map(|prn| tg6::Record { prn, ..r.clone() })
            .collect();
        a.padded_size = tg6::CAPACITY;
        let (bytes, guard) = tg6::guard(
            &a.encode().unwrap(),
            &base,
            &json!({"upload_allowed":true,"failures":[]}),
        )
        .unwrap();
        let hash = crate::camera::hash(&bytes);
        files::atomic(
            &root(temp.path()).join(format!("archives/{hash}.cep")),
            &bytes,
        )
        .unwrap();
        let mut pointer = current(temp.path()).unwrap();
        pointer["tg6"] = json!({"engine_version":1,"archive":format!("{hash}.cep"),"metrics":{},"guard":guard,"base_sha256":pointer["guard"]["output_sha256"]});
        files::json(&root(temp.path()).join("current.json"), &pointer).unwrap();
        assert_eq!(
            preflight_for(temp.path(), CameraModel::Tg1).unwrap().0,
            base
        );
        assert_eq!(
            preflight_for(temp.path(), CameraModel::Tg6).unwrap().0,
            bytes
        );
        // A TG-6-specific older clock fallback must expand only its own health
        // scope. An event outside the TG-1 fit still invalidates that TG-6 file.
        let now = snapshot.checked_at_gps;
        pointer["metrics"]["training_start_gps"] = json!(data::calendar(now - 3. * data::DAY));
        pointer["tg6"]["metrics"]["health_training_start_gps"] =
            json!(data::calendar(now - 4. * data::DAY));
        snapshot.orbit_events.push(Event {
            prn: 1,
            first_gps: now - 3.5 * data::DAY,
            last_gps: now - 3.5 * data::DAY,
            peak_innovation_m: 500.,
            samples: 1,
            reason: "clock-scope regression".into(),
        });
        files::json(&root(temp.path()).join("data/health-state.json"), &snapshot).unwrap();
        files::json(&root(temp.path()).join("current.json"), &pointer).unwrap();
        assert!(preflight_for(temp.path(), CameraModel::Tg1).is_ok());
        assert!(preflight_for(temp.path(), CameraModel::Tg6).is_err());
        snapshot.orbit_events.clear();
        files::json(&root(temp.path()).join("data/health-state.json"), &snapshot).unwrap();
        pointer["tg6"]["base_sha256"] = json!("changed");
        files::json(&root(temp.path()).join("current.json"), &pointer).unwrap();
        assert!(preflight_for(temp.path(), CameraModel::Tg6).is_err());
        assert!(preflight_for(temp.path(), CameraModel::Tg1).is_ok());
    }
    #[test]
    fn failed_refresh_revokes_previous_success_and_keeps_published_bytes() {
        let temp = Temp::new();
        let (bytes, _) = seed(temp.path());
        fs::remove_file(root(temp.path()).join("data/health-state.json")).unwrap();
        assert!(refresh(temp.path(), true, |_| {}).is_err());
        let status: Value =
            serde_json::from_slice(&fs::read(root(temp.path()).join("status.json")).unwrap())
                .unwrap();
        assert_eq!(status["refresh_ok"], false);
        assert_eq!(
            archive(temp.path(), &current(temp.path()).unwrap()).unwrap(),
            bytes
        );
        assert!(preflight(temp.path()).is_err());
    }
    #[test]
    fn clock_fallback_keeps_pre_maneuver_inputs_in_health_scope() {
        let temp = Temp::new();
        let (_, mut snapshot) = seed(temp.path());
        let now = snapshot.checked_at_gps;
        snapshot.orbit_events.push(Event {
            prn: 1,
            first_gps: now - 3.5 * data::DAY,
            last_gps: now - 3.5 * data::DAY,
            peak_innovation_m: 500.,
            samples: 1,
            reason: "disruption".into(),
        });
        files::json(&root(temp.path()).join("data/health-state.json"), &snapshot).unwrap();
        let mut pointer = current(temp.path()).unwrap();
        pointer["metrics"]["training_start_gps"] = json!(data::calendar(now - 3. * data::DAY));
        files::json(&root(temp.path()).join("current.json"), &pointer).unwrap();
        assert!(preflight(temp.path()).is_ok());
        // Fresh orbit fitting alone cannot recover a clock trained before the event.
        pointer["metrics"]["health_training_start_gps"] =
            json!(data::calendar(now - 4. * data::DAY));
        files::json(&root(temp.path()).join("current.json"), &pointer).unwrap();
        assert!(preflight(temp.path()).is_err());
    }
    #[test]
    fn changed_fallback_clock_inputs_invalidate_cached_orbits() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let temp = Temp::new();
        fs::write(temp.path().join("egm96.zip"), b"gravity").unwrap();
        fs::write(temp.path().join("finals2000A.all"), b"orientation").unwrap();
        let write_rapid = |clock: f64| {
            let mut raw = String::from("%c cc GPS\n");
            // This first clock sample is older than the newer orbit fit window.
            for day in [24, 26] {
                raw.push_str(&format!(
                    "*  2026 09 {day} 23 45 00.00000000\nPG01{:14.6}{:14.6}{:14.6}{:14.6}\n",
                    22000., 10000., 10000., clock
                ));
            }
            let path = temp
                .path()
                .join("IGS0OPSRAP_20262680000_01D_15M_ORB.SP3.gz");
            let mut gzip = GzEncoder::new(File::create(path).unwrap(), Compression::default());
            gzip.write_all(raw.as_bytes()).unwrap();
            gzip.finish().unwrap();
        };
        let cutoff = data::epoch("2026-09-28T12:00:00").unwrap();
        let mut observed = data::Observations {
            samples: vec![data::Sample {
                time: cutoff - 900.,
                positions: vec![Some([26000000., 0., 0.]); 32],
                clocks: vec![Some(1e-4); 32],
            }],
            paths: vec![],
        };
        write_rapid(1.);
        let first = fingerprint(&observed, temp.path(), cutoff).unwrap();
        write_rapid(2.);
        assert_ne!(first, fingerprint(&observed, temp.path(), cutoff).unwrap());
        let recent_only = fingerprint(&observed, temp.path(), cutoff).unwrap();
        observed.samples.insert(
            0,
            data::Sample {
                time: cutoff - 10. * data::DAY,
                positions: vec![None; 32],
                clocks: vec![Some(1e-4); 32],
            },
        );
        let with_history = fingerprint(&observed, temp.path(), cutoff).unwrap();
        assert_ne!(recent_only, with_history);
        observed.samples[0].clocks[0] = Some(2e-4);
        let changed_history = fingerprint(&observed, temp.path(), cutoff).unwrap();
        assert_ne!(with_history, changed_history);
        observed.samples.push(data::Sample {
            time: cutoff,
            positions: vec![Some([26000000., 0., 0.]); 32],
            clocks: vec![Some(1.); 32],
        });
        assert_eq!(
            changed_history,
            fingerprint(&observed, temp.path(), cutoff).unwrap()
        );
    }
    #[test]
    fn cache_publication_rejects_traversal_corruption_and_overlapping_refreshes() {
        let temp = Temp::new();
        seed(temp.path());
        let pointer = current(temp.path()).unwrap();
        let mut bad = pointer.clone();
        bad["archive"] = json!("../private");
        assert!(archive(temp.path(), &bad).is_err());
        files::atomic(
            &root(temp.path())
                .join("archives")
                .join(pointer["archive"].as_str().unwrap()),
            b"corrupt",
        )
        .unwrap();
        assert!(archive(temp.path(), &pointer).is_err());
        let first = lock(&root(temp.path())).unwrap();
        assert!(lock(&root(temp.path())).is_err());
        drop(first);
        assert!(lock(&root(temp.path())).is_ok());
    }
}
