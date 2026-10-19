// SPDX-License-Identifier: MIT
//! Native camera monitoring, prediction scheduling, uploads and commit receipts.
use crate::{
    camera,
    model::{Action, CameraInfo, CameraModel, DataInfo, Device, Phase, Receipt, State, now_gps},
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

fn source_refresh_delay(failures: u32, checked_gps: Option<f64>, gps_now: f64) -> Duration {
    if failures > 0 {
        // Recover quickly from transient network errors, then back off.
        return Duration::from_secs((30u64 << (failures - 1).min(4)).min(300));
    }
    // Refresh before the one-hour upload check expires. The source timestamp
    // precedes orbit fitting, so a long calculation must shorten this delay.
    let remaining = checked_gps.map_or(0., |checked| 2700. - (gps_now - checked));
    Duration::from_secs_f64(remaining.clamp(0., 1800.))
}

fn refresh_on_connection(device: Option<&Device>, data: &DataInfo) -> bool {
    device.is_some_and(|d| d.model.supports_gps()) && !data.upload_allowed
}

/// Vendor sessions can interrupt USB storage. Permit one automatic update per
/// USB attachment (including its status query). Busy storage waits for a retry.
#[derive(Default)]
struct CameraActivity {
    connection: String,
    unmounted_since: Option<Instant>,
    probe_attempted: bool,
    automatic_attempted: bool,
    connected_at: Option<Instant>,
    storage_released: bool,
    preparation_attempted: bool,
    waiting_for_storage: bool,
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
        !self.waiting_for_storage
            && self.unmounted_since.is_some_and(|at| {
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
            || self.waiting_for_storage
            || self.initial_expired(now)
            || self
                .connected_at
                .is_none_or(|at| now.duration_since(at) < Duration::from_secs(3))
        {
            return false;
        }
        // Busy storage restores this allowance without starting a vendor session.
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
        !self.waiting_for_storage
            && self
                .connected_at
                .is_some_and(|at| now.duration_since(at) >= Duration::from_secs(90))
    }
    fn wait_for_storage(&mut self) {
        self.waiting_for_storage = true;
        self.preparation_attempted = false;
        self.probe_attempted = false;
        self.automatic_attempted = false;
        self.unmounted_since = None;
    }
    fn retry_storage(&mut self, now: Instant) {
        self.waiting_for_storage = false;
        // Time spent waiting for a busy filesystem does not use the GPS budget.
        self.connected_at = Some(now - Duration::from_secs(3));
    }
    fn should_release(&self, now: Instant, state: &State) -> bool {
        self.automatic_attempted
            || (self.probe_attempted
                && (!state.gps_supported()
                    || !state.automatic
                    || (!state.updating_sources
                        && (!state.data.upload_allowed || state.matches_latest()))
                    || state.automatic_upload_wait().is_some()))
            || self.initial_expired(now)
            || state.multiple_cameras
            || state.reconnect_required
            || state.camera_error.is_some()
    }
    fn finished_access(&mut self) {
        // Give storage reattachment/automount time to appear before another session.
        self.unmounted_since = None;
    }
    fn rebind_after_status_check(&mut self, connection: &str) {
        // A mode change is part of the same check, not a new plug-in event.
        self.connection = connection.into();
    }
}

pub fn read_json(path: impl AsRef<Path>) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_string()
}
fn save_upload_settings(config: &Config, automatic: bool, hours: u32) -> Result<()> {
    let path = config.state_dir.join("settings.json");
    let mut settings = read_json(&path).unwrap_or_else(|_| json!({}));
    ensure!(settings.is_object(), "Invalid settings file");
    settings["automatic"] = json!(automatic);
    settings["automatic_interval_hours"] = json!(hours);
    atomic_json(&path, &settings)
}
fn archive_exclusions(data: &[u8]) -> Option<Vec<u8>> {
    use crate::predictor::validation::{BLOCK, RECORD};
    if data.len() == crate::predictor::tg6::CAPACITY {
        let archive = crate::predictor::tg6::Archive::decode(data).ok()?;
        return Some(
            (1..=32u8)
                .filter(|p| !archive.records.iter().any(|r| r.prn == *p as u16))
                .collect(),
        );
    }
    (data.len() == 4 * BLOCK).then(|| {
        (1..=32u8)
            .filter(|&prn| {
                !(0..2).any(|week| data[week * BLOCK + 6 + (prn as usize - 1) * RECORD + 1] == 0)
            })
            .collect()
    })
}
fn hydrate_receipt(config: &Config, mut receipt: Receipt) -> Receipt {
    if receipt.excluded_prns.is_none()
        && let Ok(data) = engine::archive(
            &config.state_dir,
            &json!({"archive":format!("{}.cep", receipt.sha256)}),
        )
    {
        receipt.excluded_prns = archive_exclusions(&data);
    }
    receipt
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

/// Headless access uses the same model checks, health preflight, mount handling,
/// commit receipts and transport lock as the desktop. No arbitrary payloads.
pub fn camera_cli(mut args: impl Iterator<Item = String>) -> Result<()> {
    let action = args.next().unwrap_or_default();
    if matches!(action.as_str(), "--help" | "-h" | "") {
        println!(
            "toughfix camera info [--state-dir PATH]\ntoughfix camera update [--state-dir PATH]\nUse one connected supported camera. Update sends only current health-validated predictions from a successful refresh. Storage is restored afterward."
        );
        return Ok(());
    }
    ensure!(
        matches!(action.as_str(), "info" | "update"),
        "Unknown camera action"
    );
    let mut state_dir = crate::default_state_dir();
    while let Some(arg) = args.next() {
        ensure!(arg == "--state-dir", "Unknown camera option {arg}");
        state_dir = args.next().context("--state-dir needs a path")?.into();
    }
    let config = Config {
        project: env!("CARGO_MANIFEST_DIR").into(),
        state_dir,
        demo: false,
        monitor_only: false,
    };
    let _lock = lock_instance(&config)?;
    let devices = camera::discover_checked()?;
    ensure!(
        devices.len() == 1,
        "Connect exactly one supported camera in USB Storage mode"
    );
    let device = &devices[0];
    // Reject missing/unapproved predictions before changing mount state.
    if action == "update" {
        preflight(&config, device.model)?;
    }
    let shared = Arc::new(Mutex::new(State {
        device: Some(device.clone()),
        data: data_info(&config, device.model),
        ..Default::default()
    }));
    let result=storage::prepare(device).and_then(|prepared| {
        if action=="info" {
            let info=camera::probe(&prepared)?;
            remember_camera(&config,&prepared,&info);
            println!("{}",serde_json::to_string_pretty(&json!({"model":device.model.scsi_name(),"camera":info}))?);
            Ok(())
        } else {
            upload(&config,&shared,&prepared,UploadKind::Latest)?;
            let s=shared.lock().unwrap();
            println!("{}",serde_json::to_string_pretty(&json!({"model":device.model.scsi_name(),"receipt":s.receipt,"message":s.message}))?);
            Ok(())
        }
    });
    let restored = storage::release(device);
    match (result, restored) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => {
            Err(error).context("Camera operation finished, but storage could not be restored")
        }
        (Err(error), Err(restore)) => {
            Err(error).context(format!("Storage restoration also failed: {restore:#}"))
        }
    }
}

fn latest_receipt(config: &Config) -> Option<Receipt> {
    let mut receipts: Vec<Receipt> = fs::read(config.state_dir.join("commits.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if let Some(receipt) = receipts.pop() {
        return Some(hydrate_receipt(config, receipt));
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
        excluded_prns: None,
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
    let receipt = receipt.map(|r| {
        let mut r = hydrate_receipt(config, r);
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
    s.camera_note = None;
}
/// An explicit read-only refresh does not touch the automatic connection budget
/// or prediction files. Always restore storage, even after a partial unmount.
/// Returns true when busy storage needs another attempt.
fn refresh_camera_with(
    config: &Config,
    shared: &Shared,
    prepare: impl FnOnce(&Device) -> Result<Device>,
    probe: impl FnOnce(&Device) -> Result<CameraInfo>,
    restore: impl FnOnce(&Device) -> Result<()>,
) -> bool {
    let device = {
        let mut s = shared.lock().unwrap();
        if !s.can_refresh_camera() {
            return false;
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
    let waiting = result.as_ref().err().is_some_and(storage::retryable) && restored.is_ok();
    let mut s = shared.lock().unwrap();
    s.phase = Phase::Idle;
    s.message = "Camera information refreshed".into();
    if let Err(e) = result
        && !waiting
    {
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
    waiting
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

fn data_info(config: &Config, model: CameraModel) -> DataInfo {
    let pointer = engine::current_for(&config.state_dir, model).unwrap_or_default();
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
    let start = if model == CameraModel::Tg6 {
        crate::predictor::tg6::Archive::decode(&data)
            .map(|a| a.start())
            .unwrap_or(0.)
    } else {
        data.get(..4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_be_bytes)
            .unwrap_or(0) as f64
    };
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
    let refresh_error = (status["refresh_ok"] == false).then(|| {
        status["error"]
            .as_str()
            .unwrap_or("Satellite data refresh failed")
            .to_owned()
    });
    if status["refresh_ok"] != true {
        failures.insert(
            0,
            refresh_error
                .clone()
                .unwrap_or_else(|| "Predictions need an initial refresh".into()),
        );
    }
    if !fresh {
        failures.push("Satellite health checks need refreshing".into());
    }
    if data.len() != model.assistance_size() || guard["output_sha256"].as_str() != Some(&digest) {
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
        refresh_error,
        failures,
        excluded,
        excluded_prns: archive_exclusions(&data).unwrap_or_default(),
    }
}

struct Prepared {
    data: Vec<u8>,
    sha256: String,
    start: String,
    end: String,
}
fn preflight(config: &Config, model: CameraModel) -> Result<Prepared> {
    let (data, pointer) = engine::preflight_for(&config.state_dir, model)?;
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
/// Restore every volume even when preparation stops partway through the card.
/// Returns true when busy storage needs another attempt.
fn manual_upload_with(
    shared: &Shared,
    device: &Device,
    prepare: impl FnOnce(&Device) -> Result<Device>,
    transfer: impl FnOnce(&Device) -> Result<()>,
    restore: impl FnOnce(&Device) -> Result<()>,
) -> bool {
    set_phase(shared, Phase::Checking);
    shared.lock().unwrap().storage_error = None;
    let mut started = false;
    let result = prepare(device).and_then(|prepared| {
        started = true;
        transfer(&prepared)
    });
    let phase = shared.lock().unwrap().phase.clone();
    set_phase(shared, Phase::RestoringStorage);
    let restored = restore(device);
    let waiting = result.as_ref().err().is_some_and(storage::retryable) && restored.is_ok();
    let mut s = shared.lock().unwrap();
    s.phase = Phase::Idle;
    s.storage_preparing = false;
    if let Err(e) = result
        && !waiting
    {
        s.message = format!("GPS update failed: {e:#}");
        if started && !camera::storage_in_use(&e) {
            s.phase = Phase::Failed;
            s.reconnect_required |= camera::requires_reconnect(&e) || phase.device_busy();
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
    waiting
}
#[derive(Clone, Copy, Debug, PartialEq)]
enum UploadKind {
    Automatic,
    Latest,
    Again,
}

const STORAGE_RETRY_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq)]
enum StorageOperation {
    Initial,
    RefreshCamera,
    Upload(UploadKind),
}

struct StorageRetry {
    connection: String,
    operation: StorageOperation,
    at: Instant,
}
impl StorageRetry {
    fn new(connection: &str, operation: StorageOperation, now: Instant) -> Self {
        Self {
            connection: connection.into(),
            operation,
            at: now + STORAGE_RETRY_INTERVAL,
        }
    }
    fn due(&self, connection: &str, now: Instant) -> bool {
        self.connection == connection && now >= self.at
    }
}

fn queue_storage_retry(
    pending: &mut Option<StorageRetry>,
    shared: &Shared,
    connection: &str,
    operation: StorageOperation,
) {
    let retry = StorageRetry::new(connection, operation, Instant::now());
    let mut s = shared.lock().unwrap();
    s.phase = Phase::Idle;
    s.storage_error = None;
    s.storage_retry_at = Some(retry.at);
    s.message =
        "Waiting to unmount camera storage; the queued operation will retry automatically".into();
    *pending = Some(retry);
}
impl UploadKind {
    fn should_transfer(self, state: &State) -> bool {
        match self {
            Self::Again => true,
            Self::Latest => !state.matches_latest(),
            Self::Automatic => !state.matches_latest() && state.automatic_upload_wait().is_none(),
        }
    }
}
fn upload(config: &Config, shared: &Shared, device: &Device, kind: UploadKind) -> Result<()> {
    ensure!(
        device.model.supports_gps(),
        "This camera has no GPS receiver"
    );
    {
        let s = shared.lock().unwrap();
        ensure!(
            !s.reconnect_required,
            "Reconnect the camera before another attempt"
        );
    }
    let prepared = preflight(config, device.model)?;
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
            let current = preflight(config, device.model)?;
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
            s.camera_note = None;
            s.receipt = receipts
                .into_iter()
                .rev()
                .find(|r| r.camera_key.as_ref() == Some(&info.key))
                .map(|r| hydrate_receipt(config, r));
            if let Some(receipt) = s.receipt.as_mut() {
                // Link existing history only after a successful PTP identity
                // query. This does not invent a new commit or change its time.
                receipt.usb_key = device.serial_key.clone();
                record(config, receipt)?;
            }
            Ok(kind.should_transfer(&s))
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
                excluded_prns: archive_exclusions(&prepared.data),
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
        let mut manual_refresh_key = None;
        let mut manual_upload_key = None;
        let mut storage_retry: Option<StorageRetry> = None;
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let mut refreshing = false;
        let mut refresh_interval = Duration::from_secs(3600);
        let mut refresh_failures = 0u32;
        let mut last_scan = Instant::now() - Duration::from_secs(10);
        {
            let mut s = shared.lock().unwrap();
            s.receipt = latest_receipt(&config);
            s.demo = config.demo;
            if let Ok(settings) = read_json(config.state_dir.join("settings.json")) {
                s.automatic = settings["automatic"].as_bool().unwrap_or(true);
                s.automatic_interval_hours = settings["automatic_interval_hours"]
                    .as_u64()
                    .filter(|h| (1..=168).contains(h))
                    .unwrap_or(48) as u32;
            } else {
                s.automatic = true;
            }
            if config.monitor_only {
                s.automatic = false;
            }
            if !config.demo {
                s.data = data_info(&config, CameraModel::Tg1);
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
                Ok(Action::UpdateGps) if config.demo => demo_upload(&shared),
                Ok(Action::UpdateGps) if !config.monitor_only => {
                    let mut s = shared.lock().unwrap();
                    if s.can_update_gps() {
                        s.manual_update_pending = true;
                        manual_refresh_key = Some(last_connection.clone());
                        refresh_requested = true;
                        s.message = "Checking the latest satellite data before updating GPS".into();
                    }
                }
                Ok(Action::RefreshCamera) if config.demo => demo_refresh_camera(&shared),
                Ok(Action::RefreshCamera) => camera_refresh_requested = true,
                Ok(Action::RetryUnmount) if config.demo => {
                    if shared.lock().unwrap().storage_retry_at.take().is_some() {
                        demo_upload(&shared);
                    }
                }
                Ok(Action::RetryUnmount) => {
                    if let Some(retry) = storage_retry.as_mut() {
                        retry.at = Instant::now();
                        shared.lock().unwrap().storage_retry_at = Some(retry.at);
                    }
                }
                Ok(Action::SetAutomatic(enabled)) => {
                    let hours = shared.lock().unwrap().upload_interval_hours();
                    match save_upload_settings(&config, enabled, hours) {
                        Ok(()) => {
                            shared.lock().unwrap().automatic = enabled;
                        }
                        Err(e) => {
                            shared.lock().unwrap().message =
                                format!("Could not save automatic update setting: {e:#}");
                        }
                    }
                }
                Ok(Action::SetUploadInterval(hours)) if (1..=168).contains(&hours) => {
                    let automatic = shared.lock().unwrap().automatic;
                    match save_upload_settings(&config, automatic, hours) {
                        Ok(()) => shared.lock().unwrap().automatic_interval_hours = hours,
                        Err(e) => {
                            shared.lock().unwrap().message =
                                format!("Could not save GPS upload interval: {e:#}")
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
                let due = shared
                    .lock()
                    .unwrap()
                    .storage_retry_at
                    .is_some_and(|at| Instant::now() >= at);
                if due {
                    shared.lock().unwrap().storage_retry_at = None;
                    demo_upload(&shared);
                }
                continue;
            }
            if let Ok(result) = refresh_rx.try_recv() {
                refreshing = false;
                let result: Result<Value> = result;
                refresh_failures = if result.is_ok() {
                    0
                } else {
                    refresh_failures.saturating_add(1)
                };
                let mut s = shared.lock().unwrap();
                s.updating_sources = false;
                s.source_progress = None;
                s.data = data_info(
                    &config,
                    s.device.as_ref().map_or(CameraModel::Tg1, |d| d.model),
                );
                let mut manual_already_latest = false;
                if let Some(key) = manual_refresh_key.take() {
                    manual_already_latest =
                        result.is_ok() && s.data.upload_allowed && s.matches_latest();
                    if result.is_ok() && s.data.upload_allowed && !manual_already_latest {
                        manual_upload_key = Some((key, UploadKind::Latest));
                    } else {
                        s.manual_update_pending = false;
                    }
                }
                refresh_interval =
                    source_refresh_delay(refresh_failures, s.data.checked_gps, now_gps())
                        .max(engine::source_refresh_wait(&config.state_dir, now_gps()));
                s.message = result
                    .err()
                    .map(|e| format!("Prediction refresh failed: {e:#}"))
                    .unwrap_or_else(|| "Satellite data and predictions are up to date".into());
                if manual_already_latest {
                    s.message =
                        "This camera already has the latest predictions · no upload needed".into();
                }
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
                    if !config.monitor_only
                        && !refreshing
                        && refresh_on_connection(
                            device.as_ref(),
                            &data_info(
                                &config,
                                device.as_ref().map_or(CameraModel::Tg1, |d| d.model),
                            ),
                        )
                    {
                        // A camera arriving during a failed refresh's backoff
                        // gets an immediate check, without waiting for the timer.
                        refresh_requested = true;
                    }
                    if connection.is_empty() && !last_connection.is_empty() {
                        let _ = atomic_json(
                            &config.state_dir.join("attempt.json"),
                            &json!({"pending":false,"reconnect_required":false,"disconnected":true}),
                        );
                    }
                    s.camera = None;
                    s.camera_cached = false;
                    s.storage_error = None;
                    s.storage_retry_at = None;
                    storage_retry = None;
                    manual_refresh_key = None;
                    manual_upload_key = None;
                    s.manual_update_pending = false;
                    s.camera_note = None;
                    if let Some(key) = device.as_ref().and_then(|d| d.serial_key.as_ref()) {
                        let receipts: Vec<Receipt> =
                            fs::read(config.state_dir.join("commits.json"))
                                .ok()
                                .and_then(|b| serde_json::from_slice(&b).ok())
                                .unwrap_or_default();
                        s.receipt = receipts
                            .into_iter()
                            .rev()
                            .find(|r| r.matches_usb(key))
                            .map(|r| hydrate_receipt(&config, r));
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
                s.data = data_info(
                    &config,
                    s.device.as_ref().map_or(CameraModel::Tg1, |d| d.model),
                );
                s.multiple_cameras = devices.len() > 1;
                if s.multiple_cameras {
                    s.camera_error =
                        Some("Multiple supported cameras connected; connect one at a time".into());
                }
                last_scan = Instant::now();
            }
            let mut device = shared.lock().unwrap().device.clone();
            let mut checked_8010 = false;
            camera_activity.observe(
                &last_connection,
                device.as_ref().is_some_and(|d| d.mounts.is_empty()),
                Instant::now(),
            );
            {
                let mut s = shared.lock().unwrap();
                if storage_retry.is_some() && (s.reconnect_required || s.multiple_cameras) {
                    storage_retry = None;
                    s.storage_retry_at = None;
                    s.manual_update_pending = false;
                    camera_activity.waiting_for_storage = false;
                    camera_activity.automatic_attempted = true;
                }
            }
            {
                let mut s = shared.lock().unwrap();
                s.storage_preparing =
                    !camera_activity.storage_released && !last_connection.is_empty();
            }
            let mut initial_allowed = {
                let s = shared.lock().unwrap();
                !s.reconnect_required
                    && !s.multiple_cameras
                    && (!s.gps_supported()
                        || !s.automatic
                        || config.monitor_only
                        || (!s.updating_sources
                            && !refresh_requested
                            && refresh_at.elapsed() < refresh_interval))
            };
            if storage_retry.as_ref().is_some_and(|retry| {
                let s = shared.lock().unwrap();
                device.is_some()
                    && retry.due(&last_connection, Instant::now())
                    && !s.reconnect_required
                    && !s.multiple_cameras
            }) {
                let retry = storage_retry.take().unwrap();
                shared.lock().unwrap().storage_retry_at = None;
                match retry.operation {
                    StorageOperation::Initial => {
                        camera_activity.retry_storage(Instant::now());
                        initial_allowed = true;
                    }
                    StorageOperation::RefreshCamera => camera_refresh_requested = true,
                    StorageOperation::Upload(kind) => {
                        manual_upload_key = Some((retry.connection, kind));
                    }
                }
            }
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
                        // Restore partially unmounted cards between attempts.
                        release_storage(&shared, current);
                        if storage::retryable(&e) {
                            camera_activity.wait_for_storage();
                            queue_storage_retry(
                                &mut storage_retry,
                                &shared,
                                &last_connection,
                                StorageOperation::Initial,
                            );
                        } else {
                            camera_activity.storage_released = true;
                            camera_activity.automatic_attempted = true;
                            camera_activity.probe_attempted = true;
                            shared.lock().unwrap().storage_error =
                                Some(format!("Initial camera check failed: {e:#}"));
                        }
                    }
                }
            }
            if camera_activity.preparation_attempted
                && !camera_activity.storage_released
                && device.as_ref().is_some_and(|d| !d.mounts.is_empty())
            {
                camera_activity.wait_for_storage();
                queue_storage_retry(
                    &mut storage_retry,
                    &shared,
                    &last_connection,
                    StorageOperation::Initial,
                );
            }
            {
                let mut s = shared.lock().unwrap();
                s.storage_preparing =
                    !camera_activity.storage_released && !last_connection.is_empty();
            }
            if camera_refresh_requested {
                checked_8010 = device.as_ref().is_some_and(|d| !d.model.supports_gps())
                    && shared.lock().unwrap().can_refresh_camera();
                camera_refresh_requested = false;
                let waiting = refresh_camera_with(
                    &config,
                    &shared,
                    storage::prepare,
                    camera::probe,
                    storage::release,
                );
                if waiting {
                    queue_storage_retry(
                        &mut storage_retry,
                        &shared,
                        &last_connection,
                        StorageOperation::RefreshCamera,
                    );
                }
                last_scan = Instant::now() - Duration::from_secs(3);
            }
            if let Some(device) = device.as_ref() {
                let retry_allowed = {
                    let s = shared.lock().unwrap();
                    !s.reconnect_required && !s.multiple_cameras
                };
                let status_only = {
                    let s = shared.lock().unwrap();
                    !device.model.supports_gps()
                        || !s.automatic
                        || config.monitor_only
                        || s.automatic_upload_wait().is_some()
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
                    checked_8010 = !device.model.supports_gps();
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
                                drop(s);
                                release_storage(&shared, device);
                                camera_activity.wait_for_storage();
                                queue_storage_retry(
                                    &mut storage_retry,
                                    &shared,
                                    &last_connection,
                                    StorageOperation::Initial,
                                );
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
            // Our verified 8010 status check re-enumerates USB twice. Adopt the
            // returned attachment without resetting the one-check allowance.
            if checked_8010
                && let Some(previous) = device.as_ref().filter(|d| !d.model.supports_gps())
                && let Some(current) = camera::discover().into_iter().find(|d| {
                    !d.model.supports_gps()
                        && d.connection_key == previous.connection_key
                        && d.serial_key == previous.serial_key
                })
                && let Some(connection) = camera::connection_identity(&current)
            {
                camera_activity.rebind_after_status_check(&connection);
                last_connection = connection;
                last_device = Some(current.clone());
                shared.lock().unwrap().device = Some(current.clone());
                device = Some(current);
            }
            if !config.monitor_only
                && shared.lock().unwrap().gps_supported()
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
            {
                let mut s = shared.lock().unwrap();
                s.refresh_retry_seconds =
                    (!config.monitor_only && !refreshing && refresh_failures > 0).then(|| {
                        refresh_interval
                            .saturating_sub(refresh_at.elapsed())
                            .as_secs()
                            .saturating_add(1)
                    });
            }
            if shared.lock().unwrap().quit_pending {
                release_before_quit(&shared, &mut camera_activity, last_device.as_ref());
                shared.lock().unwrap().stopped = true;
                return;
            }
            if upload_requested && !config.monitor_only && storage_retry.is_none() {
                upload_requested = false;
                manual_upload_key = Some((last_connection.clone(), UploadKind::Again));
            }
            if let Some((key, kind)) = manual_upload_key.take() {
                let current = {
                    let s = shared.lock().unwrap();
                    (!s.reconnect_required
                        && !s.multiple_cameras
                        && s.camera_error.is_none()
                        && s.gps_supported()
                        && s.data.upload_allowed
                        && !s.phase.device_busy())
                    .then(|| s.device.clone())
                    .flatten()
                };
                if let Some(current) =
                    current.filter(|d| camera::connection_identity(d).as_ref() == Some(&key))
                {
                    let waiting = manual_upload_with(
                        &shared,
                        &current,
                        storage::prepare,
                        |prepared| upload(&config, &shared, prepared, kind),
                        storage::release,
                    );
                    if waiting {
                        queue_storage_retry(
                            &mut storage_retry,
                            &shared,
                            &last_connection,
                            StorageOperation::Upload(kind),
                        );
                    }
                    camera_activity.storage_released = true;
                    camera_activity.automatic_attempted = true;
                    camera_activity.probe_attempted = true;
                    camera_activity.finished_access();
                    last_scan = Instant::now() - Duration::from_secs(3);
                } else {
                    shared.lock().unwrap().message = "Camera disconnected, changed or needs attention; press Update GPS now again after resolving it".into();
                }
                shared.lock().unwrap().manual_update_pending = storage_retry.is_some();
            }
            let (auto, ready, already) = {
                let s = shared.lock().unwrap();
                (
                    s.automatic
                        && s.gps_supported()
                        && !s.manual_update_pending
                        && s.automatic_upload_wait().is_none()
                        && !config.monitor_only
                        && camera_activity.automatic_due(Instant::now()),
                    s.data.upload_allowed
                        && s.gps_supported()
                        && !s.updating_sources
                        && s.camera_error.is_none()
                        && !s.reconnect_required
                        && !s.multiple_cameras
                        && !s.storage_mounted(),
                    s.matches_latest(),
                )
            };
            if auto
                && ready
                && !already
                && !camera_activity.initial_expired(Instant::now())
                && !config.monitor_only
                && let Some(device) = device.as_ref()
            {
                camera_activity.automatic_attempted = true;
                camera_activity.probe_attempted = true;
                shared.lock().unwrap().message = "Validating predictions before upload".into();
                match upload(&config, &shared, device, UploadKind::Automatic) {
                    Ok(()) => (),
                    Err(e) => {
                        let mut s = shared.lock().unwrap();
                        s.message = format!("{e:#}");
                        if camera::storage_in_use(&e) {
                            s.phase = Phase::Idle;
                            drop(s);
                            release_storage(&shared, device);
                            camera_activity.wait_for_storage();
                            queue_storage_retry(
                                &mut storage_retry,
                                &shared,
                                &last_connection,
                                StorageOperation::Initial,
                            );
                        } else {
                            s.reconnect_required =
                                camera::requires_reconnect(&e) || s.phase.device_busy();
                            s.phase = Phase::Failed;
                        }
                    }
                }
                camera_activity.finished_access();
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
    s.storage_retry_at = None;
    s.camera_note = None;
    s.storage_error = None;
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
        refresh_error: None,
        failures: vec![],
        excluded: "G25".into(),
        excluded_prns: vec![25],
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
    let total = shared
        .lock()
        .unwrap()
        .device
        .as_ref()
        .map_or(0, |device| device.model.assistance_size());
    if total == 0 {
        return;
    }
    for phase in [
        Phase::Checking,
        Phase::Transferring { sent: 0, total },
        Phase::Transferring { sent: 61440, total },
        Phase::Transferring {
            sent: 122880,
            total,
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
        excluded_prns: Some(s.data.excluded_prns.clone()),
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
    fn recent_camera_state() -> State {
        let now = Utc::now();
        let gps = crate::predictor::health::utc_gps(now).unwrap();
        let date = |offset: i64| {
            DateTime::from_timestamp((gps + 315964800.) as i64 + offset, 0)
                .unwrap()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        };
        State {
            device: Some(Device {
                serial_key: Some("camera".into()),
                ..Default::default()
            }),
            receipt: Some(Receipt {
                camera_key: Some("camera".into()),
                usb_key: Some("camera".into()),
                sha256: "old".into(),
                bytes: 130720,
                start_gps: date(-86400),
                end_gps: date(13 * 86400),
                committed_at: now - chrono::Duration::hours(6),
                session_closed: true,
                origin: "test".into(),
                excluded_prns: Some(vec![25]),
            }),
            data: DataInfo {
                sha256: "new".into(),
                bytes: 130720,
                excluded_prns: vec![25],
                upload_allowed: true,
                ..Default::default()
            },
            automatic: true,
            ..Default::default()
        }
    }

    #[test]
    fn waiting_for_storage_is_informational_and_preserves_the_snapshot() {
        let at = Utc::now() - chrono::Duration::hours(3);
        let mut state = State {
            device: Some(Device {
                model: crate::model::CameraModel::Tough8010,
                mounts: vec!["/media/card".into()],
                ..Default::default()
            }),
            camera: Some(CameraInfo {
                read_at: Some(at),
                battery: Some(100),
                ..Default::default()
            }),
            camera_cached: true,
            ..Default::default()
        };
        state.storage_error = Some("old warning".into());
        let shared = Arc::new(Mutex::new(state));
        let mut pending = None;
        queue_storage_retry(&mut pending, &shared, "usb", StorageOperation::Initial);
        let s = shared.lock().unwrap();
        let activity = s.activity();
        assert_eq!(activity.title, "Waiting to unmount");
        assert!(!activity.warning);
        assert!(!activity.busy);
        assert!(activity.detail.contains("30 seconds"));
        assert!(s.storage_error.is_none());
        assert!(!s.can_refresh_camera());
        assert_eq!(s.camera.as_ref().unwrap().read_at, Some(at));
    }

    #[test]
    fn busy_storage_retries_indefinitely_then_allows_the_initial_gps_update() {
        let start = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("usb", false, start);
        assert!(activity.take_initial_preparation(start + Duration::from_secs(3)));
        let state = State {
            automatic: true,
            device: Some(Device::default()),
            data: DataInfo {
                upload_allowed: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut now = start + Duration::from_secs(3);
        for _ in 0..120 {
            activity.wait_for_storage();
            let mut retry = StorageRetry::new("usb", StorageOperation::Initial, now);
            assert!(!activity.take_initial_preparation(now));
            assert!(!activity.take_probe(now));
            assert!(!activity.automatic_due(now));
            assert!(!activity.should_release(now, &state));
            assert!(!retry.due("usb", now + Duration::from_secs(29)));
            now += STORAGE_RETRY_INTERVAL;
            assert!(retry.due("usb", now));
            assert!(!retry.due("another camera", now));
            assert!(!retry.due("", now));
            // Try now uses the same deadline and operation as automatic retry.
            retry.at = now - STORAGE_RETRY_INTERVAL;
            assert!(retry.due("usb", now));
            assert_eq!(retry.operation, StorageOperation::Initial);
            activity.retry_storage(now);
            assert!(activity.take_initial_preparation(now));
        }
        activity.observe("usb", true, now);
        assert!(activity.automatic_due(now));
        assert!(activity.take_probe(now));
        activity.automatic_attempted = true;
        assert!(activity.should_release(now, &state));
        activity.storage_released = true;
        activity.observe("usb", false, now + Duration::from_secs(1));
        assert!(!activity.take_initial_preparation(now + Duration::from_secs(300)));
    }

    #[test]
    fn manual_operations_wait_for_busy_storage_and_resume_without_replaying_a_transfer() {
        let temp = crate::test_support::Temp::new();
        let config = Config {
            project: temp.path().into(),
            state_dir: temp.path().into(),
            demo: false,
            monitor_only: false,
        };
        for refresh in [false, true] {
            let device = Device::default();
            let shared = Arc::new(Mutex::new(State {
                device: Some(device.clone()),
                ..Default::default()
            }));
            let transfers = std::cell::Cell::new(0);
            let restores = std::cell::Cell::new(0);
            for busy in [true, true, false] {
                let prepare = |d: &Device| {
                    if busy {
                        Err(gtk::gio::DBusError::new_for_dbus_error(
                            "org.freedesktop.UDisks2.Error.DeviceBusy",
                            "Storage is busy",
                        )
                        .into())
                    } else {
                        Ok(d.clone())
                    }
                };
                let restore = |_: &Device| {
                    restores.set(restores.get() + 1);
                    Ok(())
                };
                let waiting = if refresh {
                    refresh_camera_with(
                        &config,
                        &shared,
                        prepare,
                        |_| {
                            transfers.set(transfers.get() + 1);
                            Ok(CameraInfo::default())
                        },
                        restore,
                    )
                } else {
                    manual_upload_with(
                        &shared,
                        &device,
                        prepare,
                        |_| {
                            transfers.set(transfers.get() + 1);
                            Ok(())
                        },
                        restore,
                    )
                };
                assert_eq!(waiting, busy);
                let s = shared.lock().unwrap();
                assert_eq!(s.phase, Phase::Idle);
                assert!(s.storage_error.is_none());
                assert!(s.camera_error.is_none());
                assert!(!s.reconnect_required);
            }
            assert_eq!(transfers.get(), 1);
            assert_eq!(restores.get(), 3);
        }
    }

    #[test]
    fn trip_update_bypasses_interval_but_identical_data_still_skips_flash() {
        let mut s = recent_camera_state();
        assert!(!UploadKind::Automatic.should_transfer(&s));
        assert!(UploadKind::Latest.should_transfer(&s));
        s.data.sha256 = "old".into();
        assert!(!UploadKind::Automatic.should_transfer(&s));
        assert!(!UploadKind::Latest.should_transfer(&s));
        assert!(UploadKind::Again.should_transfer(&s));
    }

    #[test]
    fn recent_commit_releases_storage_after_status_even_while_sources_refresh() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("usb", true, now);
        activity.probe_attempted = true;
        let mut s = recent_camera_state();
        s.updating_sources = true;
        assert!(activity.should_release(now + Duration::from_secs(3), &s));
        s.data.excluded_prns.push(12);
        assert!(!activity.should_release(now + Duration::from_secs(3), &s));
    }

    #[test]
    fn upload_preferences_preserve_interval_toggle_and_unknown_fields() {
        let temp = crate::test_support::Temp::new();
        let config = Config {
            project: temp.path().into(),
            state_dir: temp.path().into(),
            demo: false,
            monitor_only: false,
        };
        atomic_json(
            &temp.path().join("settings.json"),
            &json!({"automatic":false,"future_setting":"keep"}),
        )
        .unwrap();
        save_upload_settings(&config, false, 72).unwrap();
        save_upload_settings(&config, true, 72).unwrap();
        let settings = read_json(temp.path().join("settings.json")).unwrap();
        assert_eq!(settings["automatic"], true);
        assert_eq!(settings["automatic_interval_hours"], 72);
        assert_eq!(settings["future_setting"], "keep");
    }

    #[test]
    fn old_receipt_exclusions_are_recovered_only_from_the_exact_committed_archive() {
        let temp = crate::test_support::Temp::new();
        let config = Config {
            project: temp.path().into(),
            state_dir: temp.path().into(),
            demo: false,
            monitor_only: false,
        };
        let bytes = include_bytes!("../tests/fixtures/working-camera.cep");
        let hash = camera::hash(bytes);
        let dir = engine::root(temp.path()).join("archives");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{hash}.cep")), bytes).unwrap();
        let mut r = recent_camera_state().receipt.unwrap();
        r.sha256 = hash.clone();
        r.excluded_prns = None;
        let at = r.committed_at;
        let linked = hydrate_receipt(&config, r.clone());
        assert_eq!(linked.excluded_prns, Some(vec![25]));
        assert_eq!(linked.committed_at, at);
        fs::write(dir.join(format!("{hash}.cep")), vec![0; 130720]).unwrap();
        assert!(hydrate_receipt(&config, r).excluded_prns.is_none());
    }

    #[test]
    fn manual_gps_update_restores_card_on_partial_unmount_or_transfer_failure() {
        use std::cell::RefCell;
        for prepare_fails in [true, false] {
            let shared = Arc::new(Mutex::new(recent_camera_state()));
            let device = shared.lock().unwrap().device.clone().unwrap();
            let calls = RefCell::new(Vec::new());
            manual_upload_with(
                &shared,
                &device,
                |d| {
                    calls.borrow_mut().push("prepare");
                    if prepare_fails {
                        anyhow::bail!("Card is busy");
                    }
                    Ok(d.clone())
                },
                |_| {
                    calls.borrow_mut().push("transfer");
                    anyhow::bail!("Interrupted transfer");
                },
                |_| {
                    calls.borrow_mut().push("restore");
                    assert_eq!(shared.lock().unwrap().phase, Phase::RestoringStorage);
                    Ok(())
                },
            );
            assert_eq!(
                *calls.borrow(),
                if prepare_fails {
                    vec!["prepare", "restore"]
                } else {
                    vec!["prepare", "transfer", "restore"]
                }
            );
            let s = shared.lock().unwrap();
            assert_eq!(s.receipt.as_ref().unwrap().sha256, "old");
            assert_eq!(s.reconnect_required, !prepare_fails);
            assert!(!s.storage_preparing);
        }
    }

    #[test]
    fn refresh_schedule_preserves_health_freshness_and_recovers_with_bounded_backoff() {
        let checked = 1_474_926_000.;
        let normal = source_refresh_delay(0, Some(checked), checked + 60.);
        assert!(normal.as_secs_f64() + 60. < 3600.);
        // Fitting took forty minutes: do not wait another half hour and expire.
        assert_eq!(
            source_refresh_delay(0, Some(checked), checked + 2400.),
            Duration::from_secs(300)
        );
        assert_eq!(
            source_refresh_delay(0, Some(checked), checked + 3601.),
            Duration::ZERO
        );
        let retries: Vec<_> = (1..=8)
            .map(|n| source_refresh_delay(n, Some(checked), checked + 7200.).as_secs())
            .collect();
        assert_eq!(retries, [30, 60, 120, 240, 300, 300, 300, 300]);
        let mut data = DataInfo::default();
        let mut device = Device::default();
        assert!(refresh_on_connection(Some(&device), &data));
        data.upload_allowed = true;
        assert!(!refresh_on_connection(Some(&device), &data));
        data.upload_allowed = false;
        device.model = crate::model::CameraModel::Tough8010;
        assert!(!refresh_on_connection(Some(&device), &data));
        assert!(!refresh_on_connection(None, &data));
    }

    #[test]
    fn mode_rebinding_preserves_one_check_budget_and_real_reconnect_resets_it() {
        let mut activity = CameraActivity::default();
        let now = Instant::now();
        activity.observe("old", true, now);
        let ready = now + Duration::from_secs(4);
        assert!(activity.take_initial_preparation(ready));
        assert!(activity.take_probe(ready));
        let state = State {
            device: Some(Device {
                model: crate::model::CameraModel::Tough8010,
                ..Default::default()
            }),
            automatic: true,
            updating_sources: true,
            ..Default::default()
        };
        assert!(activity.should_release(ready, &state));
        activity.storage_released = true;
        activity.finished_access();
        activity.rebind_after_status_check("returned");
        activity.observe("returned", true, ready);
        assert!(!activity.take_initial_preparation(ready));
        assert!(!activity.take_probe(ready));
        activity.observe("reconnected", true, ready);
        let ready_again = ready + Duration::from_secs(4);
        assert!(activity.take_initial_preparation(ready_again));
        assert!(activity.take_probe(ready_again));
    }
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
            excluded_prns: None,
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
    fn completed_initial_preparation_is_not_repeated_until_reconnect() {
        let now = Instant::now();
        let mut activity = CameraActivity::default();
        activity.observe("usb-attachment", false, now);
        assert!(!activity.take_initial_preparation(now));
        assert!(activity.take_initial_preparation(now + Duration::from_secs(3)));
        // Success and non-retryable failures consume the preparation allowance.
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
                excluded_prns: None,
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
            excluded_prns: None,
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
