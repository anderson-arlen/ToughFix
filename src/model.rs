// SPDX-License-Identifier: MIT
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Idle,
    Checking,
    ReadingCamera,
    RestoringStorage,
    Transferring {
        sent: usize,
        total: usize,
    },
    Validating,
    Committing,
    Closing,
    Failed,
}

impl Phase {
    pub fn device_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking
                | Self::ReadingCamera
                | Self::RestoringStorage
                | Self::Transferring { .. }
                | Self::Validating
                | Self::Committing
                | Self::Closing
        )
    }
    pub fn label(&self) -> String {
        match self {
            Self::Idle => "No upload in progress".into(),
            Self::Checking => "Checking camera".into(),
            Self::ReadingCamera => "Refreshing camera information".into(),
            Self::RestoringStorage => "Restoring camera storage".into(),
            Self::Transferring { sent, total } => {
                format!("Uploading · {}%", sent * 100 / (*total).max(1))
            }
            Self::Validating => "Camera is validating predictions".into(),
            Self::Committing => "Saving predictions to camera".into(),
            Self::Closing => "Closing camera session".into(),
            Self::Failed => "Last operation failed".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Storage {
    pub label: String,
    pub capacity: u64,
    pub free: u64,
    pub writable: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Device {
    pub path: PathBuf,
    pub sys_path: PathBuf,
    pub connection_key: String,
    /// Only a real USB serial can identify a camera across reconnections.
    pub serial_key: Option<String>,
    pub capacity: u64,
    pub mounts: Vec<String>,
    pub mounted_storage: Vec<Storage>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CameraInfo {
    pub key: String,
    pub firmware: String,
    pub battery: Option<u8>,
    pub gps_chip: u32,
    pub transfer_limit: u32,
    pub storage: Vec<Storage>,
    pub read_at: Option<DateTime<Utc>>,
}

impl CameraInfo {
    pub fn snapshot_age_at(&self, now: DateTime<Utc>) -> String {
        let Some(at) = self.read_at else {
            return "Unknown age".into();
        };
        let seconds = now.signed_duration_since(at).num_seconds();
        if (-60..60).contains(&seconds) {
            "just now".into()
        } else {
            elapsed_age(seconds as f64)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub camera_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usb_key: Option<String>,
    pub sha256: String,
    pub bytes: usize,
    pub start_gps: String,
    pub end_gps: String,
    pub committed_at: DateTime<Utc>,
    pub session_closed: bool,
    pub origin: String,
}

impl Receipt {
    pub fn matches_usb(&self, key: &str) -> bool {
        // Early receipts stored only the PTP serial hash. An identical USB
        // serial proves that identity without opening a vendor session.
        self.usb_key.as_deref().map_or_else(
            || self.camera_key.as_deref() == Some(key),
            |linked| linked == key,
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct DataInfo {
    pub source: String,
    pub source_url: String,
    pub observed_gps: Option<f64>,
    pub fitted_observed_gps: Option<f64>,
    pub clock_summary: String,
    pub checked_gps: Option<f64>,
    pub training_end: String,
    pub training_start: String,
    pub start_gps: String,
    pub end_gps: String,
    pub sha256: String,
    pub bytes: usize,
    pub available: Vec<usize>,
    pub fit_rms: Option<f64>,
    pub decoded_rms: Option<f64>,
    pub propagation_seconds: Option<f64>,
    pub upload_allowed: bool,
    pub failures: Vec<String>,
    pub excluded: String,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub device: Option<Device>,
    pub camera: Option<CameraInfo>,
    pub camera_cached: bool,
    pub camera_error: Option<String>,
    pub multiple_cameras: bool,
    pub phase: Phase,
    pub data: DataInfo,
    pub receipt: Option<Receipt>,
    pub message: String,
    pub updating_sources: bool,
    pub source_progress: Option<crate::predictor::Progress>,
    pub storage_preparing: bool,
    pub storage_error: Option<String>,
    pub automatic: bool,
    pub quit_pending: bool,
    pub stopped: bool,
    pub reconnect_required: bool,
    pub demo: bool,
}

pub struct Activity {
    pub title: String,
    pub detail: String,
    pub busy: bool,
    pub fraction: Option<f64>,
    pub warning: bool,
}

impl State {
    pub fn can_refresh_camera(&self) -> bool {
        self.device.is_some()
            && !self.phase.device_busy()
            && !self.storage_preparing
            && !self.reconnect_required
            && !self.multiple_cameras
            && !self.quit_pending
    }
    pub fn activity(&self) -> Activity {
        let mut activity = Activity {
            title: String::new(),
            detail: String::new(),
            busy: false,
            fraction: None,
            warning: false,
        };
        if self.phase.device_busy() {
            activity.title = self.phase.label();
            activity.busy = true;
            activity.detail = match self.phase {
                Phase::Transferring { sent, total } => {
                    activity.fraction = Some(sent as f64 / total.max(1) as f64);
                    format!(
                        "{} of {} transferred · keep the camera connected",
                        transfer_bytes(sent),
                        transfer_bytes(total)
                    )
                }
                Phase::Checking => "Checking the camera and its GPS assistance update".into(),
                Phase::ReadingCamera => {
                    "Reading battery, health and SD card information · keep USB connected".into()
                }
                Phase::RestoringStorage => {
                    "Making the card available for browsing again · keep USB connected".into()
                }
                Phase::Validating => {
                    "The camera is checking the new prediction file · keep USB connected".into()
                }
                Phase::Committing => "Saving GPS assistance · do not unplug the camera".into(),
                Phase::Closing => "Finishing the camera update · keep USB connected".into(),
                _ => String::new(),
            };
            if self.quit_pending {
                activity.detail.push_str(" · will quit when finished");
            }
        } else if self.updating_sources {
            activity.busy = true;
            if let Some(progress) = self.source_progress.as_ref() {
                activity.title = progress.title().into();
                activity.detail = if let Some((completed, total)) = progress.completed {
                    activity.fraction = Some(completed as f64 / total.max(1) as f64);
                    format!("{completed} of {total} satellites processed · 14-day predictions")
                } else {
                    progress.detail.clone()
                };
            } else {
                activity.title = "Checking for GPS updates".into();
                activity.detail = "Looking up the latest official satellite data".into();
            }
        } else if self.storage_preparing {
            activity.title = "Preparing your camera".into();
            activity.detail = "Initial GPS and battery check · storage will mount afterward".into();
            activity.busy = true;
        } else if let Some(error) = self.storage_error.as_ref() {
            activity.title = "Camera storage needs attention".into();
            activity.detail = error.clone();
            activity.warning = true;
        } else if self.reconnect_required
            || self.camera_error.is_some()
            || self.phase == Phase::Failed
        {
            activity.title = "Camera update needs attention".into();
            activity.detail = self
                .camera_error
                .clone()
                .unwrap_or_else(|| self.message.clone());
            activity.warning = true;
        } else if !self.data.failures.is_empty() {
            activity.title = "GPS update unavailable".into();
            activity.detail = self.data.failures[0].clone();
            activity.warning = true;
        } else if self.device.is_none() {
            activity.title = "Waiting for your camera".into();
            activity.detail = "Connect an Olympus Tough TG-1 in USB Storage mode".into();
        } else if self.matches_latest() {
            activity.title = "Connected".into();
            activity.detail = "GPS assistance is up to date · no upload needed".into();
        } else if self.storage_mounted() {
            activity.title = "Connected".into();
            activity.detail = "Storage is ready for browsing · no camera operation running".into();
        } else {
            activity.title = "Connected".into();
            activity.detail = "ToughFix is idle".into();
        }
        activity
    }
    pub fn storage_mounted(&self) -> bool {
        self.device.as_ref().is_some_and(|d| !d.mounts.is_empty())
    }
    pub fn unplug_message(&self) -> &'static str {
        if self.phase.device_busy() {
            "Do not unplug the camera"
        } else if self.storage_preparing {
            "Do not unplug · preparing GPS assistance; storage will mount afterward"
        } else if self.reconnect_required {
            "Reconnect the camera before another update"
        } else if self.storage_mounted() {
            "Camera storage in use · GPS updates paused · eject storage before unplugging"
        } else {
            "No camera operation active"
        }
    }
    pub fn matches_latest(&self) -> bool {
        self.receipt_matches_camera()
            && self
                .receipt
                .as_ref()
                .is_some_and(|r| !self.data.sha256.is_empty() && r.sha256 == self.data.sha256)
    }
    pub fn receipt_matches_camera(&self) -> bool {
        self.device.is_some()
            && self.receipt.as_ref().is_some_and(|r| {
                if let Some(c) = self.camera.as_ref() {
                    r.camera_key.as_ref() == Some(&c.key)
                } else {
                    self.device
                        .as_ref()
                        .and_then(|d| d.serial_key.as_deref())
                        .is_some_and(|key| r.matches_usb(key))
                }
            })
    }

    pub fn assistance_summary(&self) -> String {
        if self.data.sha256.is_empty() || self.data.bytes == 0 {
            return if self.updating_sources {
                "Preparing GPS predictions"
            } else {
                "No local GPS predictions available"
            }
            .into();
        }
        let status = if !self.data.upload_allowed {
            "Predictions are not cleared for upload"
        } else if self.matches_latest() {
            "Up to date on this camera"
        } else if self.device.is_none() {
            "Predictions ready to send"
        } else if self.receipt_matches_camera() {
            "New predictions ready for this camera"
        } else {
            "Camera update status unverified"
        };
        let validity = if self.data.end_gps.is_empty() {
            String::new()
        } else {
            format!(
                "\nPredictions valid until {}",
                self.data.end_gps.get(..10).unwrap_or(&self.data.end_gps)
            )
        };
        let next = if self.device.is_some()
            && self.data.upload_allowed
            && !self.matches_latest()
            && self.storage_mounted()
        {
            if self.automatic {
                "\nEject storage and reconnect USB to check GPS."
            } else {
                "\nAutomatic updates are off · enable them in Settings."
            }
        } else {
            ""
        };
        format!("{status}{validity}{next}")
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Open,
    Quit,
    Refresh,
    RefreshCamera,
    Upload,
    SetAutomatic(bool),
    DemoConnect(bool),
    DemoUpload,
}

pub fn bytes(n: u64) -> String {
    format!("{:.1} GiB", n as f64 / 1_073_741_824.)
}

fn transfer_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KiB", n as f64 / 1024.)
    }
}

pub fn age(gps: Option<f64>) -> String {
    let Some(gps) = gps else {
        return "Unavailable".into();
    };
    let seconds = now_gps() - gps;
    elapsed_age(seconds)
}

fn elapsed_age(seconds: f64) -> String {
    if seconds < -60. {
        return "Timestamp is in the future".into();
    }
    let minutes = (seconds.max(0.) / 60.) as u64;
    if minutes < 60 {
        format!("{minutes} min ago")
    } else if minutes < 1440 {
        format!("{} h {} min ago", minutes / 60, minutes % 60)
    } else {
        format!("{} d {} h ago", minutes / 1440, minutes % 1440 / 60)
    }
}

pub fn now_gps() -> f64 {
    crate::predictor::health::now().unwrap_or(f64::NAN)
}

pub fn gps_calendar(gps: f64) -> String {
    DateTime::from_timestamp((gps + 315964800.) as i64, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M GPS").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn snapshot_age_tracks_elapsed_time_and_manual_refresh_requires_an_idle_camera() {
        let at = Utc::now();
        let camera = CameraInfo {
            read_at: Some(at),
            ..Default::default()
        };
        assert_eq!(camera.snapshot_age_at(at), "just now");
        assert_eq!(
            camera.snapshot_age_at(at + chrono::Duration::hours(1)),
            "1 h 0 min ago"
        );
        assert_eq!(CameraInfo::default().snapshot_age_at(at), "Unknown age");
        let mut s = State {
            device: Some(Device {
                mounts: vec!["/card".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            s.can_refresh_camera(),
            "Mounted storage permits an explicit normal unmount"
        );
        s.storage_preparing = true;
        assert!(!s.can_refresh_camera());
        s.storage_preparing = false;
        for phase in [
            Phase::ReadingCamera,
            Phase::RestoringStorage,
            Phase::Committing,
        ] {
            s.phase = phase;
            assert!(!s.can_refresh_camera());
            assert!(s.activity().busy);
        }
        s.phase = Phase::Idle;
        s.reconnect_required = true;
        assert!(!s.can_refresh_camera());
        s.reconnect_required = false;
        s.multiple_cameras = true;
        assert!(!s.can_refresh_camera());
        s.multiple_cameras = false;
        s.quit_pending = true;
        assert!(!s.can_refresh_camera());
    }
    #[test]
    fn old_ptp_serial_receipts_match_identical_usb_serial_without_inventing_a_commit() {
        let mut s = State {
            device: Some(Device {
                serial_key: Some("serial-hash".into()),
                mounts: vec!["/card".into()],
                ..Default::default()
            }),
            data: DataInfo {
                sha256: "archive".into(),
                bytes: 130720,
                upload_allowed: true,
                end_gps: "2026-10-14 00:00 GPS".into(),
                ..Default::default()
            },
            receipt: Some(Receipt {
                camera_key: Some("serial-hash".into()),
                usb_key: None,
                sha256: "archive".into(),
                bytes: 130720,
                start_gps: String::new(),
                end_gps: String::new(),
                committed_at: Utc::now(),
                session_closed: true,
                origin: String::new(),
            }),
            automatic: true,
            ..Default::default()
        };
        assert!(s.matches_latest());
        assert!(s.assistance_summary().contains("Up to date on this camera"));
        assert!(s.assistance_summary().contains("2026-10-14"));
        assert!(!s.assistance_summary().contains("reconnect"));
        // An explicit link takes precedence over coincidentally equal serials.
        s.receipt.as_mut().unwrap().usb_key = Some("different-USB-serial".into());
        assert!(!s.matches_latest());
        assert!(s.assistance_summary().contains("status unverified"));
        assert!(s.assistance_summary().contains("reconnect USB"));
        assert!(!s.activity().detail.contains("next connection"));
        s.receipt.as_mut().unwrap().usb_key = Some("serial-hash".into());
        s.data.sha256 = "new-archive".into();
        assert!(
            s.assistance_summary()
                .contains("New predictions ready for this camera")
        );
        s.automatic = false;
        assert!(s.assistance_summary().contains("updates are off"));
        s.device = None;
        assert!(!s.receipt_matches_camera());
        assert!(s.assistance_summary().contains("Predictions ready to send"));
        s.data.upload_allowed = false;
        assert!(s.assistance_summary().contains("not cleared"));
        s.data.sha256.clear();
        assert_eq!(s.assistance_summary(), "No local GPS predictions available");
    }
    #[test]
    fn every_refresh_stage_is_visible_as_active_work_and_calculations_report_real_counts() {
        use crate::predictor::{Progress, RefreshStage};
        let mut s = State {
            updating_sources: true,
            storage_preparing: true,
            ..Default::default()
        };
        for stage in [
            RefreshStage::SatelliteHealth,
            RefreshStage::Observations,
            RefreshStage::ModelInputs,
            RefreshStage::Calculating,
            RefreshStage::Validating,
        ] {
            s.source_progress = Some(Progress::new(stage, "Current operation"));
            let activity = s.activity();
            assert!(activity.busy);
            assert_eq!(activity.title, s.source_progress.as_ref().unwrap().title());
            assert!(activity.fraction.is_none());
        }
        s.source_progress.as_mut().unwrap().stage = RefreshStage::Calculating;
        s.source_progress.as_mut().unwrap().completed = Some((12, 32));
        let activity = s.activity();
        assert_eq!(activity.fraction, Some(0.375));
        assert!(activity.detail.contains("12 of 32"));
        s.updating_sources = false;
        assert_eq!(s.activity().title, "Preparing your camera");
        s.storage_preparing = false;
        assert!(!s.activity().busy);
    }
    #[test]
    fn camera_operations_take_priority_over_background_refresh_and_idle_has_no_progress() {
        let mut s = State {
            updating_sources: true,
            ..Default::default()
        };
        for phase in [
            Phase::Checking,
            Phase::Transferring {
                sent: 50,
                total: 100,
            },
            Phase::Validating,
            Phase::Committing,
            Phase::Closing,
        ] {
            s.phase = phase;
            assert!(s.activity().busy);
            assert_eq!(s.activity().title, s.phase.label());
        }
        s.phase = Phase::Transferring {
            sent: 50,
            total: 100,
        };
        assert_eq!(s.activity().fraction, Some(0.5));
        assert!(s.activity().detail.contains("50 B of 100 B"));
        s.phase = Phase::Transferring {
            sent: 61440,
            total: 130720,
        };
        assert!(s.activity().detail.contains("60.0 KiB of 127.7 KiB"));
        s.phase = Phase::Idle;
        s.updating_sources = false;
        assert!(!s.activity().busy);
        assert!(s.activity().fraction.is_none());
        s.phase = Phase::Failed;
        s.message = "Camera rejected the update".into();
        assert!(s.activity().warning);
        assert_eq!(s.activity().detail, s.message);
        s.phase = Phase::Idle;
        s.storage_error = Some("Open the card manually in your file manager".into());
        assert_eq!(s.activity().title, "Camera storage needs attention");
        assert!(s.activity().warning);
    }
    use super::*;
    #[test]
    fn protection_includes_commit_and_close() {
        for phase in [
            Phase::Checking,
            Phase::Transferring { sent: 1, total: 2 },
            Phase::Validating,
            Phase::Committing,
            Phase::Closing,
        ] {
            let s = State {
                phase,
                ..Default::default()
            };
            assert_eq!(s.unplug_message(), "Do not unplug the camera");
        }
        assert!(!Phase::Idle.device_busy());
    }
    #[test]
    fn a_matching_hash_on_a_different_camera_is_not_current() {
        let mut s = State {
            device: Some(Device::default()),
            camera: Some(CameraInfo {
                key: "a".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        s.data.sha256 = "hash".into();
        s.receipt = Some(Receipt {
            usb_key: None,
            camera_key: Some("b".into()),
            sha256: "hash".into(),
            bytes: 1,
            start_gps: String::new(),
            end_gps: String::new(),
            committed_at: Utc::now(),
            session_closed: true,
            origin: String::new(),
        });
        assert!(!s.matches_latest());
        s.receipt.as_mut().unwrap().camera_key = Some("a".into());
        assert!(s.matches_latest());
        s.camera = None;
        s.device = Some(Device {
            serial_key: Some("usb-a".into()),
            ..Default::default()
        });
        // Different USB/PTP serials require an explicit identity link.
        assert!(!s.matches_latest());
        s.receipt.as_mut().unwrap().usb_key = Some("usb-a".into());
        assert!(s.matches_latest());
        s.data.sha256 = "newer".into();
        assert!(!s.matches_latest());
        s.data.sha256 = "hash".into();
        s.device.as_mut().unwrap().serial_key = Some("usb-b".into());
        assert!(!s.matches_latest());
        s.device.as_mut().unwrap().serial_key = None;
        assert!(!s.matches_latest());
        s.receipt.as_mut().unwrap().usb_key = None;
        assert!(!s.matches_latest());
    }
    #[test]
    fn mounted_storage_does_not_claim_safe_unplug() {
        let s = State {
            device: Some(Device {
                path: PathBuf::new(),
                sys_path: PathBuf::new(),
                connection_key: String::new(),
                capacity: 1,
                mounts: vec!["/media/card".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(s.unplug_message().contains("eject"));
    }
}
