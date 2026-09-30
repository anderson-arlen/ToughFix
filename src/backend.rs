// SPDX-License-Identifier: MIT
//! Native camera monitoring, prediction scheduling, uploads and commit receipts.
use crate::{
    camera,
    model::{Action, CameraInfo, DataInfo, Device, Phase, Receipt, State, now_gps},
    predictor::engine,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

pub type Shared = Arc<Mutex<State>>;
#[derive(Clone)]
pub struct Config {
    pub project: PathBuf,
    pub state_dir: PathBuf,
    pub demo: bool,
    pub monitor_only: bool,
}

pub fn read_json(path: impl AsRef<Path>) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}
pub fn atomic_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    File::open(path.parent().context("Missing state directory")?)?.sync_all()?;
    Ok(())
}
pub fn lock_instance(config: &Config) -> Result<File> {
    fs::create_dir_all(&config.state_dir)?;
    fs::set_permissions(&config.state_dir, fs::Permissions::from_mode(0o700))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(config.state_dir.join("instance.lock"))?;
    // SAFETY: flock uses only the live descriptor owned by file.
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another ToughFix worker is already running"
    );
    Ok(file)
}

fn latest_receipt(config: &Config) -> Option<Receipt> {
    let mut receipts: Vec<Receipt> = fs::read(config.state_dir.join("commits.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Some(receipt) = receipts.pop() {
        return Some(receipt);
    }
    // Import the successful research trial as history, never associate it with an
    // arbitrary newly connected device. Binding requires the private identity capture.
    let root = config
        .project
        .join("research/camera/committed-upload-captures");
    let report = read_json(root.join("result.json")).ok()?;
    if report["archive_committed"] != true {
        return None;
    }
    let cmd = fs::read(root.join("commit-archive-command.bin")).ok()?;
    let response = fs::read(root.join("commit-archive-response.bin")).ok()?;
    if cmd.len() < 12 || cmd[6..8] != 0x912cu16.to_le_bytes() {
        return None;
    }
    let txn = u32::from_le_bytes(cmd[8..12].try_into().ok()?);
    if camera::parse_container(&response, 3, None, txn).ok()?.0 != 0x2001 {
        return None;
    }
    let payload: Vec<u8> = (0..3)
        .map(|i| fs::read(root.join(format!("chunk-{i:03}-data.bin"))).ok())
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .flat_map(|d| d.into_iter().skip(12))
        .collect();
    if camera::hash(&payload) != report["sha256"].as_str()? {
        return None;
    }
    let raw = fs::read(root.join("device-info-payload.bin")).ok();
    let key = raw.and_then(|b| camera::capture_key(&b).ok());
    let at = report["completed_at_gps"].as_f64()?;
    Some(Receipt {
        camera_key: key,
        sha256: text(&report, "sha256"),
        bytes: payload.len(),
        start_gps: text(&report, "start_gps"),
        end_gps: text(&report, "validity_end_exclusive_gps"),
        committed_at: DateTime::from_timestamp((at + 315964800. - 18.) as i64, 0)?,
        session_closed: report["session_closed"] == true,
        origin: "Verified research upload".into(),
    })
}

fn record(config: &Config, receipt: &Receipt) -> Result<()> {
    let path = config.state_dir.join("commits.json");
    let mut receipts: Vec<Receipt> = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        Vec::new()
    };
    if let Some(last) = receipts.last_mut().filter(|r| {
        r.camera_key == receipt.camera_key
            && r.sha256 == receipt.sha256
            && r.committed_at == receipt.committed_at
    }) {
        *last = receipt.clone();
    } else {
        receipts.push(receipt.clone());
    }
    if receipts.len() > 100 {
        receipts.drain(..receipts.len() - 100);
    }
    atomic_json(&path, &receipts)
}

fn data_info(config: &Config) -> DataInfo {
    let pointer = engine::current(&config.state_dir).unwrap_or_default();
    let metrics = &pointer["metrics"];
    let health = read_json(engine::root(&config.state_dir).join("data/health-state.json"))
        .unwrap_or_default();
    let guard = &pointer["guard"];
    let status = read_json(engine::root(&config.state_dir).join("status.json")).unwrap_or_default();
    let data = engine::archive(&config.state_dir, &pointer).unwrap_or_default();
    let digest = if data.is_empty() {
        String::new()
    } else {
        camera::hash(&data)
    };
    let start = data
        .get(..4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
        .unwrap_or(0) as f64;
    let fresh = guard["checked_at_gps"]
        .as_f64()
        .is_some_and(|t| (0. ..=3600.).contains(&(now_gps() - t)));
    let mut failures: Vec<String> = guard["failures"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if !fresh {
        failures.push("Satellite health checks need refreshing".into());
    }
    if status["refresh_ok"] != true {
        failures.push(
            status["error"]
                .as_str()
                .unwrap_or("Predictions need an initial refresh")
                .to_owned(),
        );
    }
    if data.len() != 130720 || guard["output_sha256"].as_str() != Some(&digest) {
        failures.push("Prediction file is missing or does not match its health report".into());
    }
    if !(start <= now_gps() && now_gps() < start + 1209600.) {
        failures.push("Predictions are outside their two-week window".into());
    }
    let available = guard["available_prns_by_week"]
        .as_array()
        .map(|a| a.iter().map(|r| r.as_array().map_or(0, Vec::len)).collect())
        .unwrap_or_default();
    let excluded = guard["quality_flags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v["prn"].as_u64())
                .map(|n| format!("G{n:02}"))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    DataInfo {
        source: metrics["source_data"]
            .as_str()
            .unwrap_or("IGS observed GPS orbits · NOAA public archive")
            .into(),
        source_url: "https://noaa-cors-pds.s3.amazonaws.com/".into(),
        observed_gps: health["latest_observed_gps"].as_f64(),
        fitted_observed_gps: metrics["latest_fitted_observed_gps_seconds"].as_f64(),
        clock_summary: metrics["clock_sources"]
            .as_object()
            .map(|sources| {
                sources
                    .iter()
                    .map(|(k, v)| {
                        format!(
                            "{} satellites: {}",
                            v,
                            if k == "rapid" {
                                "Rapid clocks"
                            } else {
                                "newer observed clocks"
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_default(),
        checked_gps: health["checked_at_gps"].as_f64(),
        training_end: text(metrics, "training_end_gps"),
        training_start: text(metrics, "training_start_gps"),
        start_gps: text(metrics, "forecast_start_gps"),
        end_gps: crate::model::gps_calendar(start + 1209600.),
        sha256: digest,
        bytes: data.len(),
        available,
        fit_rms: metrics["fit"]["fit_3d_rms_m"].as_f64(),
        decoded_rms: metrics["broadcast_approximation"]["rms_m"].as_f64(),
        propagation_seconds: metrics["total_seconds"].as_f64(),
        upload_allowed: guard["upload_allowed"] == true && failures.is_empty(),
        failures,
        excluded,
    }
}

struct Prepared {
    data: Vec<u8>,
    sha256: String,
    start: String,
    end: String,
}
fn preflight(config: &Config) -> Result<Prepared> {
    let (data, pointer) = engine::preflight(&config.state_dir)?;
    let sha256 = camera::hash(&data);
    Ok(Prepared {
        data,
        sha256,
        start: text(&pointer["metrics"], "start_gps"),
        end: text(&pointer["metrics"], "validity_end_exclusive_gps"),
    })
}

fn set_phase(shared: &Shared, phase: Phase) {
    shared.lock().unwrap().phase = phase;
}
fn upload(config: &Config, shared: &Shared, device: &Device) -> Result<()> {
    {
        let s = shared.lock().unwrap();
        ensure!(
            !s.reconnect_required,
            "Reconnect the camera before another attempt"
        );
    }
    let prepared = preflight(config)?;
    atomic_json(
        &config.state_dir.join("attempt.json"),
        &json!({"pending":true,"sha256":prepared.sha256,"connection_key":device.connection_key}),
    )?;
    let result = camera::upload(
        device,
        &prepared.data,
        |phase| set_phase(shared, phase),
        || {
            let current = preflight(config)?;
            ensure!(
                current.data == prepared.data && current.sha256 == prepared.sha256,
                "Predictions changed while checking camera"
            );
            ensure!(
                !shared.lock().unwrap().quit_pending,
                "Quit requested before upload started"
            );
            Ok(())
        },
        |info| {
            let receipt = Receipt {
                camera_key: Some(info.key.clone()),
                sha256: prepared.sha256.clone(),
                bytes: prepared.data.len(),
                start_gps: prepared.start.clone(),
                end_gps: prepared.end.clone(),
                committed_at: Utc::now(),
                session_closed: false,
                origin: "Rust desktop uploader".into(),
            };
            {
                let mut s = shared.lock().unwrap();
                s.receipt = Some(receipt.clone());
                s.camera = Some(info.clone());
            }
            // Preserve an acknowledged commit even if recording or session close fails.
            record(config, &receipt)
        },
    );
    if result.is_ok() {
        let mut s = shared.lock().unwrap();
        if let Some(r) = s.receipt.as_mut() {
            r.session_closed = true;
            record(config, r)?;
        }
        s.message = "Predictions committed to camera".into();
        s.phase = Phase::Idle;
    }
    atomic_json(
        &config.state_dir.join("attempt.json"),
        &json!({"pending":false,"reconnect_required":result.is_err(),"connection_key":device.connection_key,
        "error":result.as_ref().err().map(|e|format!("{e:#}"))}),
    )?;
    result
}

pub fn start(config: Config, shared: Shared, rx: mpsc::Receiver<Action>) {
    thread::spawn(move || {
        let mut last_connection = String::new();
        let mut tried_candidate = String::new();
        let mut probe_at = Instant::now() - Duration::from_secs(60);
        let mut refresh_at = Instant::now() - Duration::from_secs(3600);
        let mut refresh_requested = false;
        let mut upload_requested = false;
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let mut refreshing = false;
        let mut refresh_interval = Duration::from_secs(3600);
        let mut last_scan = Instant::now() - Duration::from_secs(10);
        {
            let mut s = shared.lock().unwrap();
            s.receipt = latest_receipt(&config);
            s.demo = config.demo;
            if let Ok(settings) = read_json(config.state_dir.join("settings.json")) {
                s.automatic = settings["automatic"].as_bool().unwrap_or(true);
            } else {
                s.automatic = true;
            }
            if config.monitor_only {
                s.automatic = false;
            }
            if !config.demo {
                s.data = data_info(&config);
            }
            s.message = if config.demo {
                "Demo · no device access or uploads"
            } else {
                "Waiting for a camera"
            }
            .into();
        }
        if config.demo {
            demo_connection(&shared, true);
        }
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Action::Quit) => {
                    shared.lock().unwrap().stopped = true;
                    return;
                }
                Ok(Action::Refresh) => {
                    refresh_requested = true;
                    probe_at = Instant::now() - Duration::from_secs(60);
                }
                Ok(Action::Upload) => upload_requested = true,
                Ok(Action::SetAutomatic(enabled)) => {
                    match atomic_json(
                        &config.state_dir.join("settings.json"),
                        &json!({"automatic":enabled}),
                    ) {
                        Ok(()) => {
                            shared.lock().unwrap().automatic = enabled;
                            tried_candidate.clear();
                        }
                        Err(e) => {
                            shared.lock().unwrap().message =
                                format!("Could not save automatic update setting: {e:#}");
                        }
                    }
                }
                Ok(Action::DemoConnect(connected)) if config.demo => {
                    demo_connection(&shared, connected)
                }
                Ok(Action::DemoUpload) if config.demo => demo_upload(&shared),
                Ok(_) => (),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    shared.lock().unwrap().stopped = true;
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
            }
            if shared.lock().unwrap().quit_pending {
                shared.lock().unwrap().stopped = true;
                return;
            }
            if config.demo {
                continue;
            }
            if let Ok(result) = refresh_rx.try_recv() {
                refreshing = false;
                let result: Result<Value> = result;
                refresh_interval = Duration::from_secs(if result.is_ok() { 3600 } else { 300 });
                let mut s = shared.lock().unwrap();
                s.updating_sources = false;
                s.data = data_info(&config);
                s.message = result
                    .err()
                    .map(|e| format!("Prediction refresh failed: {e:#}"))
                    .unwrap_or_else(|| "Satellite data and predictions are up to date".into());
                refresh_at = Instant::now();
            }
            if last_scan.elapsed() >= Duration::from_secs(2) {
                let devices = camera::discover();
                let device = devices.first().cloned();
                let connection = device
                    .as_ref()
                    .map(|d| format!("{}:{}", d.connection_key, d.sys_path.display()))
                    .unwrap_or_default();
                let mut s = shared.lock().unwrap();
                if connection != last_connection {
                    if connection.is_empty() && !last_connection.is_empty() {
                        let _ = atomic_json(
                            &config.state_dir.join("attempt.json"),
                            &json!({"pending":false,"reconnect_required":false,"disconnected":true}),
                        );
                    }
                    s.camera = None;
                    s.camera_error = None;
                    s.phase = Phase::Idle;
                    s.reconnect_required = false;
                    tried_candidate.clear();
                    probe_at = Instant::now() - Duration::from_secs(60);
                    if let Ok(attempt) = read_json(config.state_dir.join("attempt.json"))
                        && attempt["connection_key"].as_str()
                            == device.as_ref().map(|d| d.connection_key.as_str())
                        && (attempt["pending"] == true || attempt["reconnect_required"] == true)
                    {
                        s.reconnect_required = true;
                        s.message =
                            "Previous camera operation was interrupted; reconnect before retrying"
                                .into();
                    }
                    last_connection = connection;
                }
                s.device = device;
                s.data = data_info(&config);
                s.multiple_cameras = devices.len() > 1;
                if s.multiple_cameras {
                    s.camera_error =
                        Some("Multiple TG-1 cameras connected; connect one at a time".into());
                }
                last_scan = Instant::now();
            }
            let device = shared.lock().unwrap().device.clone();
            if let Some(device) = device.as_ref() {
                let retry_allowed = {
                    let s = shared.lock().unwrap();
                    !s.reconnect_required && !s.multiple_cameras
                };
                if retry_allowed && probe_at.elapsed() >= Duration::from_secs(60) {
                    set_phase(&shared, Phase::Checking);
                    match camera::probe(device) {
                        Ok(info) => {
                            let mut s = shared.lock().unwrap();
                            let receipts: Vec<Receipt> =
                                fs::read(config.state_dir.join("commits.json"))
                                    .ok()
                                    .and_then(|b| serde_json::from_slice(&b).ok())
                                    .unwrap_or_default();
                            if let Some(r) = receipts
                                .iter()
                                .rev()
                                .find(|r| r.camera_key.as_ref() == Some(&info.key))
                            {
                                s.receipt = Some(r.clone());
                            }
                            s.camera = Some(info);
                            s.camera_error = None;
                        }
                        Err(e) => {
                            let mut s = shared.lock().unwrap();
                            s.camera_error = Some(format!("{e:#}"));
                            if camera::requires_reconnect(&e) {
                                s.reconnect_required = true;
                            }
                        }
                    }
                    set_phase(&shared, Phase::Idle);
                    probe_at = Instant::now();
                }
            }
            if !config.monitor_only
                && !refreshing
                && !shared.lock().unwrap().phase.device_busy()
                && (refresh_requested || refresh_at.elapsed() >= refresh_interval)
            {
                refreshing = true;
                shared.lock().unwrap().updating_sources = true;
                let state_dir = config.state_dir.clone();
                let shared = shared.clone();
                let refresh_tx = refresh_tx.clone();
                thread::spawn(move || {
                    let result = engine::refresh(&state_dir, false, |message| {
                        shared.lock().unwrap().message = message
                    });
                    let _ = refresh_tx.send(result);
                });
                refresh_requested = false;
            }
            if shared.lock().unwrap().quit_pending {
                shared.lock().unwrap().stopped = true;
                return;
            }
            let (auto, ready, already, key) = {
                let s = shared.lock().unwrap();
                (
                    s.automatic && !config.monitor_only,
                    s.data.upload_allowed
                        && !s.updating_sources
                        && s.camera.is_some()
                        && s.camera_error.is_none()
                        && !s.reconnect_required
                        && !s.multiple_cameras,
                    s.matches_latest(),
                    s.data.sha256.clone(),
                )
            };
            if (upload_requested || (auto && ready && !already && key != tried_candidate))
                && !config.monitor_only
            {
                upload_requested = false;
                if let Some(device) = device.as_ref() {
                    if ready {
                        tried_candidate = key;
                        shared.lock().unwrap().message =
                            "Validating predictions before upload".into();
                        match upload(&config, &shared, device) {
                            Ok(()) => (),
                            Err(e) => {
                                let mut s = shared.lock().unwrap();
                                s.message = format!("{e:#}");
                                s.reconnect_required =
                                    camera::requires_reconnect(&e) || s.phase.device_busy();
                                s.phase = Phase::Failed;
                            }
                        }
                    } else {
                        shared.lock().unwrap().message =
                            "Upload blocked until camera and prediction checks pass".into();
                    }
                }
            }
        }
    });
}

fn demo_connection(shared: &Shared, connected: bool) {
    let mut s = shared.lock().unwrap();
    s.device = connected.then(|| Device {
        path: "/dev/demo-camera".into(),
        sys_path: "demo".into(),
        connection_key: "demo".into(),
        capacity: 32014073856,
        mounts: vec![],
    });
    s.camera = connected.then(|| CameraInfo {
        key: "demo".into(),
        firmware: "1.00".into(),
        battery: Some(84),
        gps_chip: 1,
        transfer_limit: 131072,
        storage: vec![crate::model::Storage {
            label: "SD card".into(),
            capacity: 32014073856,
            free: 29278076928,
            writable: true,
        }],
        read_at: Some(Utc::now()),
    });
    s.data = DataInfo {
        source: "IGS observed GPS orbits · NOAA public archive".into(),
        source_url: "https://noaa-cors-pds.s3.amazonaws.com/".into(),
        observed_gps: Some(now_gps() - 7200.),
        fitted_observed_gps: Some(now_gps() - 7200.),
        clock_summary: "31 satellites: newer observed clocks".into(),
        checked_gps: Some(now_gps() - 120.),
        training_start: "2026-09-25T14:00:00".into(),
        training_end: "2026-09-28T13:45:00".into(),
        start_gps: "2026-09-28T14:00:00".into(),
        end_gps: "2026-10-12 14:00 GPS".into(),
        sha256: "5673e761113a0d82db517262da24c990fc759c4b34fbdc77ee81595b4782ce4b".into(),
        bytes: 130720,
        available: vec![31, 31, 0, 0],
        fit_rms: Some(0.18),
        decoded_rms: Some(1.43),
        propagation_seconds: Some(43.1),
        upload_allowed: true,
        failures: vec![],
        excluded: "G25".into(),
    };
    s.message = if connected {
        "Demo camera connected"
    } else {
        "Demo camera disconnected"
    }
    .into();
    s.receipt = None;
}
fn demo_upload(shared: &Shared) {
    if shared.lock().unwrap().device.is_none() {
        return;
    }
    for phase in [
        Phase::Checking,
        Phase::Transferring {
            sent: 0,
            total: 130720,
        },
        Phase::Transferring {
            sent: 61440,
            total: 130720,
        },
        Phase::Transferring {
            sent: 122880,
            total: 130720,
        },
        Phase::Validating,
        Phase::Committing,
        Phase::Closing,
    ] {
        set_phase(shared, phase);
        thread::sleep(Duration::from_millis(700));
    }
    let mut s = shared.lock().unwrap();
    s.receipt = Some(Receipt {
        camera_key: Some("demo".into()),
        sha256: s.data.sha256.clone(),
        bytes: s.data.bytes,
        start_gps: s.data.start_gps.clone(),
        end_gps: s.data.end_gps.clone(),
        committed_at: Utc::now(),
        session_closed: true,
        origin: "Demo only · no camera write".into(),
    });
    s.phase = Phase::Idle;
    s.message = "Demo commit completed · no camera was written".into();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commit_receipts_round_trip_and_keep_camera_associations() {
        let root = crate::test_support::Temp::new();
        let c = Config {
            project: root.path().join("no-checkout"),
            state_dir: root.path().into(),
            demo: false,
            monitor_only: true,
        };
        assert!(latest_receipt(&c).is_none());
        let mut receipt = Receipt {
            camera_key: Some("camera-a".into()),
            sha256: "a".repeat(64),
            bytes: 130720,
            start_gps: "2026-09-28T14:00:00".into(),
            end_gps: "2026-10-12T14:00:00".into(),
            committed_at: Utc::now(),
            session_closed: false,
            origin: "Native Rust upload".into(),
        };
        record(&c, &receipt).unwrap();
        receipt.session_closed = true;
        record(&c, &receipt).unwrap();
        assert!(latest_receipt(&c).unwrap().session_closed);
        receipt.camera_key = Some("camera-b".into());
        record(&c, &receipt).unwrap();
        assert_eq!(
            latest_receipt(&c).unwrap().camera_key.as_deref(),
            Some("camera-b")
        );
        let receipts: Vec<Receipt> =
            serde_json::from_slice(&fs::read(c.state_dir.join("commits.json")).unwrap()).unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].camera_key.as_deref(), Some("camera-a"));
    }
}
