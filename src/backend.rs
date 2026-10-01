// SPDX-License-Identifier: MIT
//! Native camera monitoring, prediction scheduling, uploads and commit receipts.
use crate::{
    camera,
    model::{Action, CameraInfo, DataInfo, Device, Phase, Receipt, State, now_gps},
    predictor::engine,
    storage,
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

/// Vendor sessions can interrupt USB storage. Permit one automatic update per
/// USB attachment (including its status query), with one initial normal unmount.
#[derive(Default)]
struct CameraActivity {
    connection: String,
    unmounted_since: Option<Instant>,
    probe_attempted: bool,
    automatic_attempted: bool,
    connected_at: Option<Instant>,
    storage_released: bool,
    preparation_attempted: bool,
}
impl CameraActivity {
    fn observe(&mut self, connection: &str, storage_available: bool, now: Instant) {
        if self.connection != connection {
            *self = Self {
                connection: connection.into(),
                connected_at: (!connection.is_empty()).then_some(now),
                ..Default::default()
            };
        }
        if connection.is_empty() || !storage_available {
            self.unmounted_since = None;
        } else if self.unmounted_since.is_none() {
            self.unmounted_since = Some(now);
        }
    }
    fn can_access(&self, now: Instant) -> bool {
        self.unmounted_since.is_some_and(|at| {
            // UDisks Unmount returns after the filesystem is unmounted. Open
            // the guarded transport immediately, rather than leaving another
            // automount race window after an explicit preparation.
            self.preparation_attempted || now.duration_since(at) >= Duration::from_secs(3)
        })
    }
    fn take_initial_preparation(&mut self, now: Instant) -> bool {
        if self.connection.is_empty()
            || self.storage_released
            || self.preparation_attempted
            || self.initial_expired(now)
            || !self
                .connected_at
                .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(3))
        {
            return false;
        }
        // Consume the allowance before doing I/O, including a failed unmount.
        self.preparation_attempted = true;
        true
    }
    fn take_probe(&mut self, now: Instant) -> bool {
        if self.storage_released
            || self.probe_attempted
            || self.initial_expired(now)
            || !self.can_access(now)
        {
            return false;
        }
        self.probe_attempted = true;
        true
    }
    fn automatic_due(&self, now: Instant) -> bool {
        !self.storage_released
            && !self.automatic_attempted
            && !self.initial_expired(now)
            && self.can_access(now)
    }
    fn initial_expired(&self, now: Instant) -> bool {
        self.connected_at
            .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(90))
    }
    fn should_release(&self, now: Instant, state: &State) -> bool {
        self.automatic_attempted
            || (state.automatic
                && !state.updating_sources
                && state.data.upload_allowed
                && state.matches_latest()
                && self.probe_attempted)
            || (!state.automatic && self.probe_attempted)
            || (!state.updating_sources && !state.data.upload_allowed && self.probe_attempted)
            || self.initial_expired(now)
            || state.multiple_cameras
            || state.reconnect_required
            || state.camera_error.is_some()
    }
    fn finished_access(&mut self) {
        // Give storage reattachment/automount time to appear before another session.
        self.unmounted_since = None;
    }
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
        usb_key: None,
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

#[derive(serde::Serialize, serde::Deserialize)]
struct CameraSnapshot {
    usb_key: String,
    info: CameraInfo,
}
fn cached_camera(config: &Config, device: &Device) -> Option<CameraInfo> {
    let key = device.serial_key.as_ref()?;
    let snapshots: Vec<CameraSnapshot> =
        serde_json::from_slice(&fs::read(config.state_dir.join("camera-info.json")).ok()?).ok()?;
    snapshots
        .into_iter()
        .find(|s| &s.usb_key == key && !s.info.key.is_empty())
        .map(|s| s.info)
}
fn remember_camera(config: &Config, device: &Device, info: &CameraInfo) {
    let Some(key) = device.serial_key.as_ref() else {
        return;
    };
    let save = {
        let path = config.state_dir.join("camera-info.json");
        let mut snapshots: Vec<CameraSnapshot> = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        snapshots.retain(|s| &s.usb_key != key);
        snapshots.push(CameraSnapshot {
            usb_key: key.clone(),
            info: info.clone(),
        });
        if snapshots.len() > 100 {
            snapshots.drain(..snapshots.len() - 100);
        }
        atomic_json(&path, &snapshots)
    };
    if let Err(e) = save {
        eprintln!("ToughFix: could not save camera health snapshot: {e:#}");
    }
}

fn accept_camera_info(config: &Config, shared: &Shared, device: &Device, info: CameraInfo) {
    remember_camera(config, device, &info);
    let receipts: Vec<Receipt> = fs::read(config.state_dir.join("commits.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let receipt = receipts
        .into_iter()
        .rev()
        .find(|r| r.camera_key.as_ref() == Some(&info.key));
    let receipt = receipt.map(|mut r| {
        r.usb_key = device.serial_key.clone();
        if let Err(e) = record(config, &r) {
            eprintln!("ToughFix: could not link upload history: {e:#}");
        }
        r
    });
    let mut s = shared.lock().unwrap();
    s.receipt = receipt;
    s.camera = Some(info);
    s.camera_cached = false;
    s.camera_error = None;
}

/// An explicit read-only refresh does not touch the automatic connection budget
/// or prediction files. Always restore storage, even after a partial unmount.
fn refresh_camera_with(
    config: &Config,
    shared: &Shared,
    prepare: impl FnOnce(&Device) -> Result<Device>,
    probe: impl FnOnce(&Device) -> Result<CameraInfo>,
    restore: impl FnOnce(&Device) -> Result<()>,
) {
    let device = {
        let mut s = shared.lock().unwrap();
        if !s.can_refresh_camera() {
            return;
        }
        s.phase = Phase::ReadingCamera;
        s.storage_error = None;
        s.device.clone().unwrap()
    };
    let mut probe_failed = false;
    let result = prepare(&device).and_then(|prepared| {
        probe(&prepared)
            .map(|info| {
                accept_camera_info(config, shared, &prepared, info);
            })
            .inspect_err(|_| {
                probe_failed = true;
            })
    });
    set_phase(shared, Phase::RestoringStorage);
    let restored = restore(&device);
    let mut s = shared.lock().unwrap();
    s.phase = Phase::Idle;
    s.message = "Camera information refreshed".into();
    if let Err(e) = result {
        s.phase = Phase::Failed;
        s.message = format!("Camera refresh failed: {e:#}");
        if probe_failed && !camera::storage_in_use(&e) {
            s.camera_error = Some(format!("{e:#}"));
            s.reconnect_required |= camera::requires_reconnect(&e);
        } else {
            s.storage_error = Some(s.message.clone());
        }
    }
    if let Err(e) = restored {
        let error =
            format!("Could not restore camera storage: {e:#}. Open the card in your file manager.");
        s.message.push_str(&format!("\n{error}"));
        s.storage_error = Some(error);
    }
    if s.reconnect_required {
        let _ = atomic_json(
            &config.state_dir.join("attempt.json"),
            &json!({
                "pending": false, "reconnect_required": true, "connection_key": device.connection_key
            }),
        );
    }
}

fn record(config: &Config, receipt: &Receipt) -> Result<()> {
    let path = config.state_dir.join("commits.json");
    let mut receipts: Vec<Receipt> = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        Vec::new()
    };
    if let Some(last) = receipts.iter_mut().rev().find(|r| {
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
fn release_storage(shared: &Shared, device: &Device) {
    let result = storage::release(device);
    let mut s = shared.lock().unwrap();
    s.storage_preparing = false;
    s.storage_error = None;
    if let Err(e) = result
        && camera::connection_identity(device).is_some()
    {
        let error = format!(
            "Could not mount camera storage automatically: {e:#}. Open the card in your file manager."
        );
        s.message.push_str(&format!("\n{error}"));
        s.storage_error = Some(error);
    }
}

fn release_before_quit(shared: &Shared, activity: &mut CameraActivity, device: Option<&Device>) {
    if !activity.storage_released
        && let Some(device) = device
    {
        activity.storage_released = true;
        release_storage(shared, device);
    }
}
fn upload(config: &Config, shared: &Shared, device: &Device, force: bool) -> Result<()> {
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
        |info| {
            remember_camera(config, device, info);
            let current = preflight(config)?;
            ensure!(
                current.data == prepared.data && current.sha256 == prepared.sha256,
                "Predictions changed while checking camera"
            );
            ensure!(
                !shared.lock().unwrap().quit_pending,
                "Quit requested before upload started"
            );
            let receipts: Vec<Receipt> = fs::read(config.state_dir.join("commits.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let mut s = shared.lock().unwrap();
            s.camera = Some(info.clone());
            s.camera_cached = false;
            s.receipt = receipts
                .into_iter()
                .rev()
                .find(|r| r.camera_key.as_ref() == Some(&info.key));
            if let Some(receipt) = s.receipt.as_mut() {
                // Link existing history only after a successful PTP identity
                // query. This does not invent a new commit or change its time.
                receipt.usb_key = device.serial_key.clone();
                record(config, receipt)?;
            }
            Ok(force || !s.matches_latest())
        },
        |info| {
            let receipt = Receipt {
                usb_key: device.serial_key.clone(),
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
                s.camera_cached = false;
            }
            // Preserve an acknowledged commit even if recording or session close fails.
            record(config, &receipt)
        },
    );
    if result.is_ok() {
        let mut s = shared.lock().unwrap();
        if result
            .as_ref()
            .is_ok_and(|outcome| *outcome == camera::UploadOutcome::Committed)
            && let Some(r) = s.receipt.as_mut()
        {
            r.session_closed = true;
            record(config, r)?;
        }
        s.message = if result
            .as_ref()
            .is_ok_and(|outcome| *outcome == camera::UploadOutcome::Committed)
        {
            "Predictions committed to camera"
        } else {
            "This camera's last confirmed commit already matches the latest predictions"
        }
        .into();
        s.phase = Phase::Idle;
    }
    atomic_json(
        &config.state_dir.join("attempt.json"),
        &json!({"pending":false,"reconnect_required":result.as_ref().err().is_some_and(|e| !camera::storage_in_use(e)),"connection_key":device.connection_key,
        "error":result.as_ref().err().map(|e|format!("{e:#}"))}),
    )?;
    result.map(|_| ())
}

pub fn start(config: Config, shared: Shared, rx: mpsc::Receiver<Action>) {
    thread::spawn(move || {
        let mut last_connection = String::new();
        let mut last_device = None;
        let mut camera_activity = CameraActivity::default();
        let mut refresh_at = Instant::now() - Duration::from_secs(3600);
        let mut refresh_requested = false;
        let mut upload_requested = false;
        let mut camera_refresh_requested = false;
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
                    release_before_quit(&shared, &mut camera_activity, last_device.as_ref());
                    shared.lock().unwrap().stopped = true;
                    return;
                }
                Ok(Action::Refresh) => {
                    refresh_requested = true;
                }
                Ok(Action::Upload) => upload_requested = true,
                Ok(Action::RefreshCamera) if config.demo => demo_refresh_camera(&shared),
                Ok(Action::RefreshCamera) => camera_refresh_requested = true,
                Ok(Action::SetAutomatic(enabled)) => {
                    match atomic_json(
                        &config.state_dir.join("settings.json"),
                        &json!({"automatic":enabled}),
                    ) {
                        Ok(()) => {
                            shared.lock().unwrap().automatic = enabled;
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
                    release_before_quit(&shared, &mut camera_activity, last_device.as_ref());
                    shared.lock().unwrap().stopped = true;
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => (),
            }
            if shared.lock().unwrap().quit_pending {
                release_before_quit(&shared, &mut camera_activity, last_device.as_ref());
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
                s.source_progress = None;
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
                    .or(last_device.as_ref())
                    .and_then(camera::connection_identity)
                    .unwrap_or_default();
                if let Some(d) = device.as_ref() {
                    last_device = Some(d.clone());
                } else if connection.is_empty() {
                    last_device = None;
                }
                let mut s = shared.lock().unwrap();
                if connection != last_connection {
                    if connection.is_empty() && !last_connection.is_empty() {
                        let _ = atomic_json(
                            &config.state_dir.join("attempt.json"),
                            &json!({"pending":false,"reconnect_required":false,"disconnected":true}),
                        );
                    }
                    s.camera = None;
                    s.camera_cached = false;
                    s.storage_error = None;
                    if let Some(key) = device.as_ref().and_then(|d| d.serial_key.as_ref()) {
                        let receipts: Vec<Receipt> =
                            fs::read(config.state_dir.join("commits.json"))
                                .ok()
                                .and_then(|b| serde_json::from_slice(&b).ok())
                                .unwrap_or_default();
                        s.receipt = receipts.into_iter().rev().find(|r| r.matches_usb(key));
                    }
                    if let Some(device) = device.as_ref() {
                        s.camera = cached_camera(&config, device);
                        s.camera_cached = s.camera.is_some();
                    }
                    s.camera_error = None;
                    s.phase = Phase::Idle;
                    s.reconnect_required = false;
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
            let mut device = shared.lock().unwrap().device.clone();
            camera_activity.observe(
                &last_connection,
                device.as_ref().is_some_and(|d| d.mounts.is_empty()),
                Instant::now(),
            );
            {
                let mut s = shared.lock().unwrap();
                s.storage_preparing =
                    !camera_activity.storage_released && !last_connection.is_empty();
            }
            let initial_allowed = {
                let s = shared.lock().unwrap();
                !s.reconnect_required
                    && !s.multiple_cameras
                    && (!s.automatic
                        || config.monitor_only
                        || (!s.updating_sources && refresh_at.elapsed() < refresh_interval))
            };
            if initial_allowed
                && let Some(current) = device.as_ref()
                && camera_activity.take_initial_preparation(Instant::now())
            {
                match storage::prepare(current) {
                    Ok(prepared) => {
                        eprintln!(
                            "ToughFix: initial storage preparation completed; checking camera"
                        );
                        last_device = Some(prepared.clone());
                        shared.lock().unwrap().device = Some(prepared.clone());
                        device = Some(prepared);
                        camera_activity.observe(&last_connection, true, Instant::now());
                    }
                    Err(e) => {
                        eprintln!("ToughFix: initial storage preparation failed: {e:#}");
                        // Restore any partitions already unmounted, and never
                        // retry this connection while the user is browsing.
                        camera_activity.storage_released = true;
                        camera_activity.automatic_attempted = true;
                        camera_activity.probe_attempted = true;
                        release_storage(&shared, current);
                        let mut s = shared.lock().unwrap();
                        s.storage_error = Some(format!(
                            "Initial camera check skipped: {e:#}. Storage will be left alone until the next connection."
                        ));
                    }
                }
            }
            if camera_activity.preparation_attempted
                && !camera_activity.storage_released
                && device.as_ref().is_some_and(|d| !d.mounts.is_empty())
            {
                // Storage mounted again after the one preparation attempt.
                // Never take it away a second time on this USB attachment.
                camera_activity.storage_released = true;
                camera_activity.automatic_attempted = true;
                camera_activity.probe_attempted = true;
                let mut s = shared.lock().unwrap();
                let message = "Storage mounted again before camera access; initial check skipped. Storage will be left alone until the next connection.";
                eprintln!("ToughFix: {message}");
                s.storage_error = Some(message.into());
            }
            {
                let mut s = shared.lock().unwrap();
                s.storage_preparing =
                    !camera_activity.storage_released && !last_connection.is_empty();
            }
            if camera_refresh_requested {
                camera_refresh_requested = false;
                refresh_camera_with(
                    &config,
                    &shared,
                    storage::prepare,
                    camera::probe,
                    storage::release,
                );
                last_scan = Instant::now() - Duration::from_secs(3);
            }
            if let Some(device) = device.as_ref() {
                let retry_allowed = {
                    let s = shared.lock().unwrap();
                    !s.reconnect_required && !s.multiple_cameras
                };
                let status_only = {
                    let s = shared.lock().unwrap();
                    !s.automatic
                        || config.monitor_only
                        || (!s.updating_sources && s.data.upload_allowed && s.matches_latest())
                        || (!s.updating_sources
                            && !s.data.upload_allowed
                            && refresh_at.elapsed() < refresh_interval)
                };
                if retry_allowed
                    && status_only
                    && !upload_requested
                    && camera_activity.take_probe(Instant::now())
                {
                    set_phase(&shared, Phase::Checking);
                    eprintln!("ToughFix: reading initial camera health and battery");
                    match camera::probe(device) {
                        Ok(info) => {
                            eprintln!(
                                "ToughFix: camera health read succeeded; battery {:?}",
                                info.battery
                            );
                            accept_camera_info(&config, &shared, device, info);
                        }
                        Err(e) => {
                            eprintln!("ToughFix: initial camera health read failed: {e:#}");
                            let mut s = shared.lock().unwrap();
                            if camera::storage_in_use(&e) {
                                // Automount won the race; no camera command ran.
                                camera_activity.probe_attempted = false;
                                s.camera_error = None;
                                s.message = e.to_string();
                            } else {
                                s.camera_error = Some(format!("{e:#}"));
                                if camera::requires_reconnect(&e) {
                                    s.reconnect_required = true;
                                }
                            }
                        }
                    }
                    set_phase(&shared, Phase::Idle);
                    camera_activity.finished_access();
                }
            }
            if !config.monitor_only
                && !refreshing
                && !shared.lock().unwrap().phase.device_busy()
                && (refresh_requested || refresh_at.elapsed() >= refresh_interval)
            {
                refreshing = true;
                {
                    let mut s = shared.lock().unwrap();
                    s.updating_sources = true;
                    s.source_progress = Some(crate::predictor::Progress::new(
                        crate::predictor::RefreshStage::SatelliteHealth,
                        "Starting the satellite data check",
                    ));
                }
                let state_dir = config.state_dir.clone();
                let shared = shared.clone();
                let refresh_tx = refresh_tx.clone();
                thread::spawn(move || {
                    let result = engine::refresh(&state_dir, false, |progress| {
                        let mut s = shared.lock().unwrap();
                        let regressed = s.source_progress.as_ref().is_some_and(|old| {
                            old.stage == progress.stage
                                && old
                                    .completed
                                    .zip(progress.completed)
                                    .is_some_and(|((a, _), (b, _))| a > b)
                        });
                        if !regressed {
                            s.message = progress.detail.clone();
                            s.source_progress = Some(progress);
                        }
                    });
                    let _ = refresh_tx.send(result);
                });
                refresh_requested = false;
            }
            if shared.lock().unwrap().quit_pending {
                release_before_quit(&shared, &mut camera_activity, last_device.as_ref());
                shared.lock().unwrap().stopped = true;
                return;
            }
            let (auto, ready, already) = {
                let s = shared.lock().unwrap();
                (
                    s.automatic
                        && !config.monitor_only
                        && camera_activity.automatic_due(Instant::now()),
                    s.data.upload_allowed
                        && !s.updating_sources
                        && s.camera_error.is_none()
                        && !s.reconnect_required
                        && !s.multiple_cameras
                        && !s.storage_mounted(),
                    s.matches_latest(),
                )
            };
            if (upload_requested
                || (auto && ready && !already && !camera_activity.initial_expired(Instant::now())))
                && !config.monitor_only
            {
                let force = upload_requested;
                upload_requested = false;
                if let Some(device) = device.as_ref() {
                    if ready && camera_activity.can_access(Instant::now()) {
                        camera_activity.automatic_attempted = true;
                        camera_activity.probe_attempted = true;
                        shared.lock().unwrap().message =
                            "Validating predictions before upload".into();
                        match upload(&config, &shared, device, force) {
                            Ok(()) => (),
                            Err(e) => {
                                let mut s = shared.lock().unwrap();
                                s.message = format!("{e:#}");
                                if camera::storage_in_use(&e) {
                                    camera_activity.probe_attempted = s.camera.is_some();
                                    s.phase = Phase::Idle;
                                } else {
                                    s.reconnect_required =
                                        camera::requires_reconnect(&e) || s.phase.device_busy();
                                    s.phase = Phase::Failed;
                                }
                            }
                        }
                        camera_activity.finished_access();
                    } else {
                        shared.lock().unwrap().message = if !device.mounts.is_empty() {
                            "Unmount camera storage to update GPS assistance".into()
                        } else {
                            "Upload blocked until camera and prediction checks pass".into()
                        };
                    }
                }
            }
            if !camera_activity.storage_released
                && let Some(device) = device.as_ref()
            {
                let s = shared.lock().unwrap();
                let finished = camera_activity.should_release(Instant::now(), &s);
                drop(s);
                if finished {
                    camera_activity.storage_released = true;
                    camera_activity.automatic_attempted = true;
                    camera_activity.probe_attempted = true;
                    if camera_activity.initial_expired(Instant::now()) {
                        shared.lock().unwrap().message = "Initial GPS update timed out; storage is available. Predictions continue refreshing for the next connection.".into();
                    }
                    if shared.lock().unwrap().multiple_cameras {
                        for camera in camera::discover() {
                            release_storage(&shared, &camera);
                        }
                    } else {
                        release_storage(&shared, device);
                    }
                }
            }
        }
    });
}

fn demo_connection(shared: &Shared, connected: bool) {
    let mut s = shared.lock().unwrap();
    s.camera_cached = false;
    s.device = connected.then(|| Device {
        path: "/dev/demo-camera".into(),
        sys_path: "demo".into(),
        connection_key: "demo".into(),
        serial_key: Some("demo".into()),
        capacity: 32014073856,
        mounts: vec![],
        ..Default::default()
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
        usb_key: None,
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

fn demo_refresh_camera(shared: &Shared) {
    if !shared.lock().unwrap().can_refresh_camera() {
        return;
    }
    set_phase(shared, Phase::ReadingCamera);
    thread::sleep(Duration::from_millis(700));
    {
        let mut s = shared.lock().unwrap();
        if let Some(camera) = s.camera.as_mut() {
            camera.read_at = Some(Utc::now());
        }
        s.camera_cached = false;
    }
    set_phase(shared, Phase::RestoringStorage);
    thread::sleep(Duration::from_millis(700));
    set_phase(shared, Phase::Idle);
}

#[cfg(test)]
mod tests {
    #[test]
    fn manual_refresh_restores_storage_and_preserves_commit_without_reopening_automatic_window() {
        let temp = crate::test_support::Temp::new();
        let config = Config {
            project: temp.path().into(),
            state_dir: temp.path().into(),
            demo: false,
            monitor_only: false,
        };
        let device = Device {
            mounts: vec!["/mounted/card".into()],
            serial_key: Some("usb".into()),
            ..Default::default()
        };
        let at = Utc::now() - chrono::Duration::hours(1);
        let receipt = Receipt {
            camera_key: Some("camera".into()),
            usb_key: Some("usb".into()),
            sha256: "unchanged".into(),
            bytes: 130720,
            start_gps: String::new(),
            end_gps: String::new(),
            committed_at: at,
            session_closed: true,
            origin: "Previous upload".into(),
        };
        record(&config, &receipt).unwrap();
        let shared = Arc::new(Mutex::new(State {
            device: Some(device.clone()),
            camera: Some(CameraInfo {
                read_at: Some(at),
                battery: Some(100),
                ..Default::default()
            }),
            receipt: Some(receipt),
            ..Default::default()
        }));
        let calls = std::cell::RefCell::new(Vec::new());
        refresh_camera_with(
            &config,
            &shared,
            |d| {
                calls.borrow_mut().push("unmount");
                assert_eq!(d.mounts.len(), 1);
                Ok(Device {
                    mounts: vec![],
                    ..d.clone()
                })
            },
            |d| {
                calls.borrow_mut().push("probe");
                assert!(d.mounts.is_empty());
                assert_eq!(shared.lock().unwrap().phase, Phase::ReadingCamera);
                Ok(CameraInfo {
                    key: "camera".into(),
                    battery: Some(75),
                    read_at: Some(Utc::now()),
                    ..Default::default()
                })
            },
            |_| {
                calls.borrow_mut().push("remount");
                assert_eq!(shared.lock().unwrap().phase, Phase::RestoringStorage);
                Ok(())
            },
        );
        assert_eq!(*calls.borrow(), ["unmount", "probe", "remount"]);
        let s = shared.lock().unwrap();
        assert_eq!(s.camera.as_ref().unwrap().battery, Some(75));
        assert!(s.camera.as_ref().unwrap().read_at.unwrap() > at);
        assert_eq!(s.receipt.as_ref().unwrap().committed_at, at);
        assert_eq!(s.receipt.as_ref().unwrap().sha256, "unchanged");
        assert_eq!(s.phase, Phase::Idle);
        drop(s);
        // A manual operation never resets the automatic connection allowance.
        let now = Instant::now();
        let mut activity = CameraActivity {
            connection: "usb".into(),
            connected_at: Some(now),
            storage_released: true,
            probe_attempted: true,
            automatic_attempted: true,
            preparation_attempted: true,
            ..Default::default()
        };
        activity.observe("usb", true, now + Duration::from_secs(10));
        assert!(!activity.take_initial_preparation(now + Duration::from_secs(10)));
        assert!(!activity.take_probe(now + Duration::from_secs(10)));
        assert!(!activity.automatic_due(now + Duration::from_secs(10)));
    }

    #[test]
    fn manual_refresh_failures_restore_storage_and_retain_snapshot_age() {
        let temp = crate::test_support::Temp::new();
        let config = Config {
            project: temp.path().into(),
            state_dir: temp.path().into(),
            demo: false,
            monitor_only: false,
        };
        let at = Utc::now() - chrono::Duration::hours(1);
        for unmount_failed in [true, false] {
            let shared = Arc::new(Mutex::new(State {
                device: Some(Device::default()),
                camera: Some(CameraInfo {
                    read_at: Some(at),
                    battery: Some(80),
                    ..Default::default()
                }),
                ..Default::default()
            }));
            let restored = std::cell::Cell::new(false);
            refresh_camera_with(
                &config,
                &shared,
                |d| {
                    if unmount_failed {
                        anyhow::bail!("Card is busy")
                    } else {
                        Ok(d.clone())
                    }
                },
                |_| {
                    assert!(
                        !unmount_failed,
                        "Busy storage must prevent all camera commands"
                    );
                    anyhow::bail!("Camera rejected query")
                },
                |_| {
                    restored.set(true);
                    if unmount_failed {
                        Ok(())
                    } else {
                        anyhow::bail!("Mount failed")
                    }
                },
            );
            let s = shared.lock().unwrap();
            assert!(restored.get());
            assert_eq!(s.camera.as_ref().unwrap().read_at, Some(at));
            assert_eq!(s.phase, Phase::Failed);
            assert!(!s.reconnect_required);
            assert_eq!(s.camera_error.is_none(), unmount_failed);
            assert!(s.storage_error.is_some());
            if !unmount_failed {
                assert!(s.message.contains("Mount failed"));
            }
        }
    }

    #[test]
    fn initial_unmount_allowance_is_consumed_even_on_failure_and_only_reconnect_resets_it() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("usb-attachment", false, now);
        assert!(!activity.take_initial_preparation(now));
        assert!(activity.take_initial_preparation(now + Duration::from_secs(3)));
        // Both a failed attempt and a successful unmount/remount consume it.
        for unmounted in [false, true, false, true] {
            activity.observe("usb-attachment", unmounted, now + Duration::from_secs(10));
            assert!(!activity.take_initial_preparation(now + Duration::from_secs(10)));
        }
        activity.storage_released = true;
        activity.observe("usb-attachment", true, now + Duration::from_secs(20));
        assert!(!activity.take_probe(now + Duration::from_secs(25)));
        assert!(!activity.automatic_due(now + Duration::from_secs(25)));
        activity.observe("usb-new-attachment", false, now + Duration::from_secs(30));
        assert!(activity.take_initial_preparation(now + Duration::from_secs(33)));
    }
    #[test]
    fn early_automount_does_not_consume_initial_check_and_preparation_allows_immediate_access() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        // Detection wins initially, then the desktop mounts before we are ready.
        activity.observe("usb-attachment", true, now);
        assert!(!activity.take_initial_preparation(now));
        activity.observe("usb-attachment", false, now + Duration::from_secs(1));
        assert!(!activity.take_initial_preparation(now + Duration::from_secs(1)));
        assert!(!activity.storage_released);
        let ready = now + Duration::from_secs(4);
        assert!(activity.take_initial_preparation(ready));
        activity.observe("usb-attachment", true, ready);
        // The normal unmount has completed: no extra three-second race window.
        assert!(activity.take_probe(ready));
        assert!(activity.should_release(ready, &State::default()));
        activity.storage_released = true;
        activity.observe("usb-attachment", false, ready + Duration::from_secs(1));
        activity.observe("usb-attachment", true, ready + Duration::from_secs(100));
        assert!(!activity.take_initial_preparation(ready + Duration::from_secs(100)));
        assert!(!activity.take_probe(ready + Duration::from_secs(100)));
        assert!(!activity.automatic_due(ready + Duration::from_secs(100)));
    }
    #[test]
    fn late_storage_availability_does_not_start_an_initial_operation() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("usb-attachment", false, now);
        activity.observe("usb-attachment", true, now + Duration::from_secs(100));
        let later = now + Duration::from_secs(105);
        assert!(!activity.take_initial_preparation(later));
        assert!(!activity.take_probe(later));
        assert!(!activity.automatic_due(later));
    }
    #[test]
    fn unchanged_forecast_reads_health_once_before_mounting_and_never_repeats_while_browsing() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("attachment", true, now);
        let mut state = State {
            automatic: true,
            device: Some(Device {
                serial_key: Some("camera".into()),
                ..Default::default()
            }),
            data: DataInfo {
                sha256: "unchanged".into(),
                upload_allowed: true,
                ..Default::default()
            },
            receipt: Some(Receipt {
                camera_key: Some("camera".into()),
                usb_key: None,
                sha256: "unchanged".into(),
                bytes: 1,
                start_gps: String::new(),
                end_gps: String::new(),
                committed_at: Utc::now(),
                session_closed: true,
                origin: String::new(),
            }),
            ..Default::default()
        };
        assert!(state.matches_latest());
        assert!(!activity.should_release(now, &state));
        assert!(!activity.take_probe(now + Duration::from_secs(2)));
        assert!(activity.take_probe(now + Duration::from_secs(3)));
        assert!(activity.should_release(now + Duration::from_secs(3), &state));
        activity.storage_released = true;
        activity.observe("attachment", false, now + Duration::from_secs(4));
        state.device.as_mut().unwrap().mounts.push("/card".into());
        assert!(!activity.take_probe(now + Duration::from_secs(1000)));
        assert!(!activity.automatic_due(now + Duration::from_secs(1000)));
    }
    #[test]
    fn health_snapshot_persists_only_for_its_verified_usb_serial() {
        let root = crate::test_support::Temp::new();
        let config = Config {
            project: root.path().into(),
            state_dir: root.path().into(),
            demo: false,
            monitor_only: true,
        };
        let mut device = Device {
            serial_key: Some("usb-camera-A".into()),
            ..Default::default()
        };
        let info = CameraInfo {
            key: "PTP-camera-A".into(),
            battery: Some(84),
            read_at: Some(Utc::now()),
            ..Default::default()
        };
        remember_camera(&config, &device, &info);
        let cached = cached_camera(&config, &device).unwrap();
        assert_eq!(cached.key, info.key);
        assert_eq!(cached.battery, Some(84));
        assert_eq!(cached.read_at, info.read_at);
        device.serial_key = Some("usb-camera-B".into());
        assert!(cached_camera(&config, &device).is_none());
        device.serial_key = None;
        remember_camera(&config, &device, &info);
        assert!(cached_camera(&config, &device).is_none());
        let snapshots: Vec<CameraSnapshot> =
            serde_json::from_slice(&fs::read(config.state_dir.join("camera-info.json")).unwrap())
                .unwrap();
        assert_eq!(snapshots.len(), 1);
    }
    #[test]
    fn initial_storage_hold_is_bounded_and_never_restarts_for_later_forecasts() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("camera", true, now);
        let mut state = State {
            automatic: true,
            updating_sources: true,
            ..Default::default()
        };
        assert!(!activity.should_release(now + Duration::from_secs(30), &state));
        assert!(activity.should_release(now + Duration::from_secs(90), &state));
        activity.storage_released = true;
        state.updating_sources = false;
        state.data.upload_allowed = true;
        activity.observe("camera", true, now + Duration::from_secs(120));
        assert!(!activity.automatic_due(now + Duration::from_secs(125)));
        assert!(!activity.take_probe(now + Duration::from_secs(125)));
        activity.observe("new-attachment", true, now + Duration::from_secs(180));
        assert!(activity.automatic_due(now + Duration::from_secs(185)));
    }
    #[test]
    fn failed_refresh_and_disabled_updates_release_storage_without_an_upload() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("camera", true, now);
        let state = State {
            automatic: true,
            ..Default::default()
        };
        assert!(!activity.should_release(now, &state));
        assert!(activity.take_probe(now + Duration::from_secs(3)));
        assert!(activity.should_release(now + Duration::from_secs(3), &state));
        activity.probe_attempted = false;
        let state = State::default();
        assert!(!activity.should_release(now, &state));
        assert!(activity.take_probe(now + Duration::from_secs(3)));
        assert!(activity.should_release(now + Duration::from_secs(3), &state));
    }
    #[test]
    fn browsing_storage_never_schedules_vendor_sessions_and_remounts_do_not_reset_limits() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        for second in 0..20 {
            let time = now + Duration::from_secs(second);
            activity.observe("camera:usb-attachment", false, time);
            assert!(!activity.take_probe(time));
            assert!(!activity.automatic_due(time));
        }
        let unmounted = now + Duration::from_secs(20);
        activity.observe("camera:usb-attachment", true, unmounted);
        assert!(!activity.take_probe(unmounted + Duration::from_secs(2)));
        assert!(activity.take_probe(unmounted + Duration::from_secs(3)));
        activity.finished_access();
        // A probe-induced disk disappearance is not a new USB attachment.
        activity.observe(
            "camera:usb-attachment",
            false,
            unmounted + Duration::from_secs(4),
        );
        activity.observe(
            "camera:usb-attachment",
            true,
            unmounted + Duration::from_secs(10),
        );
        assert!(!activity.take_probe(unmounted + Duration::from_secs(13)));
        assert!(activity.automatic_due(unmounted + Duration::from_secs(13)));
        activity.automatic_attempted = true;
        for hour in 1..25 {
            let time = unmounted + Duration::from_secs(hour * 3600);
            activity.observe("camera:usb-attachment", true, time);
            assert!(!activity.take_probe(time));
            assert!(!activity.automatic_due(time));
        }
        activity.observe("", false, unmounted + Duration::from_secs(90000));
        activity.observe(
            "camera:new-attachment",
            true,
            unmounted + Duration::from_secs(90001),
        );
        assert!(activity.take_probe(unmounted + Duration::from_secs(90004)));
        assert!(activity.automatic_due(unmounted + Duration::from_secs(90004)));
    }
    #[test]
    fn automount_cancels_pending_camera_access_and_requires_another_unmounted_pause() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("camera", true, now);
        activity.observe("camera", false, now + Duration::from_secs(2));
        assert!(!activity.take_probe(now + Duration::from_secs(60)));
        assert!(!activity.automatic_due(now + Duration::from_secs(60)));
        activity.observe("camera", true, now + Duration::from_secs(61));
        assert!(!activity.take_probe(now + Duration::from_secs(63)));
        assert!(activity.take_probe(now + Duration::from_secs(64)));
    }

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
            usb_key: None,
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
        // Linking old history to a USB identity must not create a new commit.
        let mut linked = receipts[0].clone();
        linked.usb_key = Some("usb-a".into());
        record(&c, &linked).unwrap();
        let receipts: Vec<Receipt> =
            serde_json::from_slice(&fs::read(c.state_dir.join("commits.json")).unwrap()).unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].usb_key.as_deref(), Some("usb-a"));
        assert_eq!(receipts[0].committed_at, linked.committed_at);
    }
}
