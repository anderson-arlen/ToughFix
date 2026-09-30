// SPDX-License-Identifier: MIT
use anyhow::{Context, Result};
use std::{fs, path::PathBuf};

/// The user service checks this same marker before launching on a USB event.
#[derive(Clone)]
pub struct CameraStartup {
    config: PathBuf,
}

impl CameraStartup {
    pub fn at(config: PathBuf) -> Result<Self> {
        anyhow::ensure!(
            config.is_absolute(),
            "Desktop settings directory must be absolute"
        );
        Ok(Self { config })
    }

    pub fn for_user() -> Result<Self> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join(".config"))
            })
            .context("Cannot locate your desktop settings directory")?;
        Self::at(config)
    }

    pub fn installed(&self) -> bool {
        self.config
            .join("systemd/user/toughfix-camera.service")
            .is_file()
    }

    fn marker(&self) -> PathBuf {
        self.config.join("toughfix/camera-start-disabled")
    }

    pub fn enabled(&self) -> Result<bool> {
        // Metadata follows symlinks just like the service's `test -e`.
        match fs::metadata(self.marker()) {
            Ok(_) => Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e).context("Cannot read camera startup preference"),
        }
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<()> {
        let marker = self.marker();
        if enabled {
            match fs::remove_file(marker) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).context("Cannot enable camera startup"),
            }
        } else {
            fs::create_dir_all(marker.parent().unwrap())?;
            // Existence alone is the preference: no partial JSON can be read.
            fs::File::create(marker)?.sync_all()?;
            Ok(())
        }
    }
}

pub fn should_exit(
    hotplug: bool,
    visible: bool,
    busy: bool,
    refreshing: bool,
    absent_seconds: u64,
    seen_camera: bool,
) -> bool {
    hotplug
        && !visible
        && !busy
        && !refreshing
        && absent_seconds >= if seen_camera { 5 } else { 30 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn connection_startup_choice_survives_reopen() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let config = std::env::temp_dir().join(format!(
            "toughfix-startup-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let startup = CameraStartup {
            config: config.clone(),
        };
        assert!(!startup.installed());
        assert!(startup.enabled().unwrap());
        startup.set_enabled(false).unwrap();
        let reopened = CameraStartup {
            config: config.clone(),
        };
        assert!(!reopened.enabled().unwrap());
        reopened.set_enabled(true).unwrap();
        reopened.set_enabled(true).unwrap();
        assert!(startup.enabled().unwrap());
        fs::remove_dir_all(config).unwrap();
    }

    #[test]
    fn hidden_connected_instance_exits_only_when_idle_and_window_closed() {
        assert!(!should_exit(true, false, false, false, 29, false));
        assert!(should_exit(true, false, false, false, 30, false));
        assert!(!should_exit(true, true, false, false, 60, false));
        assert!(!should_exit(true, false, true, false, 60, false));
        assert!(!should_exit(true, false, false, true, 60, false));
        assert!(!should_exit(false, false, false, false, 60, false));
        assert!(should_exit(true, false, false, false, 5, true));
    }
}
