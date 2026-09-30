// SPDX-License-Identifier: MIT
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Phase {
    #[default]
    Idle,
    Checking,
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
            Self::Transferring { sent, total } => {
                format!("Uploading · {}%", sent * 100 / (*total).max(1))
            }
            Self::Validating => "Camera is validating predictions".into(),
            Self::Committing => "Saving predictions to camera".into(),
            Self::Closing => "Closing camera session".into(),
            Self::Failed => "Last operation failed".into(),
        }
    }
    pub fn fraction(&self) -> f64 {
        match self {
            Self::Transferring { sent, total } => 0.8 * *sent as f64 / (*total).max(1) as f64,
            Self::Validating => 0.85,
            Self::Committing => 0.92,
            Self::Closing => 0.98,
            _ => 0.,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Storage {
    pub label: String,
    pub capacity: u64,
    pub free: u64,
    pub writable: bool,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub path: PathBuf,
    pub sys_path: PathBuf,
    pub connection_key: String,
    pub capacity: u64,
    pub mounts: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct CameraInfo {
    pub key: String,
    pub firmware: String,
    pub battery: Option<u8>,
    pub gps_chip: u32,
    pub transfer_limit: u32,
    pub storage: Vec<Storage>,
    pub read_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub camera_key: Option<String>,
    pub sha256: String,
    pub bytes: usize,
    pub start_gps: String,
    pub end_gps: String,
    pub committed_at: DateTime<Utc>,
    pub session_closed: bool,
    pub origin: String,
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
    pub camera_error: Option<String>,
    pub multiple_cameras: bool,
    pub phase: Phase,
    pub data: DataInfo,
    pub receipt: Option<Receipt>,
    pub message: String,
    pub updating_sources: bool,
    pub automatic: bool,
    pub quit_pending: bool,
    pub stopped: bool,
    pub reconnect_required: bool,
    pub demo: bool,
}

impl State {
    pub fn unplug_message(&self) -> &'static str {
        if self.phase.device_busy() {
            "Do not unplug the camera"
        } else if self.reconnect_required {
            "Reconnect the camera before another update"
        } else if self.device.as_ref().is_some_and(|d| !d.mounts.is_empty()) {
            "No camera operation active · eject mounted storage before unplugging"
        } else {
            "No camera operation active"
        }
    }
    pub fn matches_latest(&self) -> bool {
        self.camera
            .as_ref()
            .zip(self.receipt.as_ref())
            .is_some_and(|(c, r)| {
                r.camera_key.as_ref() == Some(&c.key)
                    && !self.data.sha256.is_empty()
                    && r.sha256 == self.data.sha256
            })
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Open,
    Quit,
    Refresh,
    Upload,
    SetAutomatic(bool),
    DemoConnect(bool),
    DemoUpload,
}

pub fn bytes(n: u64) -> String {
    format!("{:.1} GiB", n as f64 / 1_073_741_824.)
}

pub fn age(gps: Option<f64>) -> String {
    let Some(gps) = gps else {
        return "Unavailable".into();
    };
    let seconds = now_gps() - gps;
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
            camera: Some(CameraInfo {
                key: "a".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        s.data.sha256 = "hash".into();
        s.receipt = Some(Receipt {
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
            }),
            ..Default::default()
        };
        assert!(s.unplug_message().contains("eject"));
    }
}
