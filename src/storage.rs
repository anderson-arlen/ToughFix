// SPDX-License-Identifier: MIT
//! Prepare and restore storage for the initial camera check through UDisks.
use crate::{backend, camera, model::Device};
use anyhow::{Context, Result, bail, ensure};
use gtk::{gio, glib, prelude::*};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const BUS: &str = "org.freedesktop.UDisks2";
const BLOCK: &str = "org.freedesktop.UDisks2.Block";
const FILESYSTEM: &str = "org.freedesktop.UDisks2.Filesystem";

fn object_path(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .context("Missing block device name")?
        .as_encoded_bytes();
    let mut escaped = String::new();
    for &byte in name {
        if byte.is_ascii_alphanumeric() {
            escaped.push(byte as char);
        } else {
            escaped.push_str(&format!("_{byte:02x}"));
        }
    }
    Ok(format!("/org/freedesktop/UDisks2/block_devices/{escaped}"))
}

trait MountClient {
    fn mount(&mut self, path: &Path) -> Result<()>;
    fn unmount(&mut self, path: &Path) -> Result<()>;
}
struct UDisks(gio::DBusConnection);
impl UDisks {
    fn is_mounted(&self, path: &Path) -> Result<bool> {
        let object = object_path(path)?;
        let points = self.0.call_sync(
            Some(BUS),
            &object,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(FILESYSTEM, "MountPoints").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            None::<&gio::Cancellable>,
        )?;
        let (points,) = points
            .get::<(glib::Variant,)>()
            .context("Invalid UDisks MountPoints")?;
        ensure!(
            points.type_().as_str() == "aay",
            "Invalid UDisks MountPoints type"
        );
        Ok(points.n_children() != 0)
    }
}
impl MountClient for UDisks {
    fn unmount(&mut self, path: &Path) -> Result<()> {
        if !self.is_mounted(path)? {
            return Ok(());
        }
        let object = object_path(path)?;
        // Never force an unmount: open files must make this attempt fail safely.
        let options = HashMap::from([("auth.no_user_interaction".to_owned(), true.to_variant())]);
        self.0.call_sync(
            Some(BUS),
            &object,
            FILESYSTEM,
            "Unmount",
            Some(&(options,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            10000,
            None::<&gio::Cancellable>,
        )?;
        Ok(())
    }
    fn mount(&mut self, path: &Path) -> Result<()> {
        let object = object_path(path)?;
        if self.is_mounted(path)? {
            return Ok(());
        }
        let usage = self.0.call_sync(
            Some(BUS),
            &object,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(BLOCK, "IdUsage").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2000,
            None::<&gio::Cancellable>,
        )?;
        let (usage,) = usage
            .get::<(glib::Variant,)>()
            .context("Invalid UDisks IdUsage")?;
        ensure!(
            usage.str() == Some("filesystem"),
            "No filesystem found on {}",
            path.display()
        );
        let options = HashMap::from([("auth.no_user_interaction".to_owned(), true.to_variant())]);
        self.0.call_sync(
            Some(BUS),
            &object,
            FILESYSTEM,
            "Mount",
            Some(&(options,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            10000,
            None::<&gio::Cancellable>,
        )?;
        Ok(())
    }
}

fn volumes(device: &Device) -> Result<Vec<PathBuf>> {
    let mut volumes = Vec::new();
    for entry in fs::read_dir(&device.sys_path)? {
        let entry = entry?;
        if entry.path().join("partition").is_file() {
            volumes.push(
                device
                    .path
                    .parent()
                    .context("Missing device directory")?
                    .join(entry.file_name()),
            );
        }
    }
    if volumes.is_empty() {
        volumes.push(device.path.clone());
    }
    volumes.sort();
    Ok(volumes)
}

fn mount_volumes(device: &Device, client: &mut impl MountClient) -> Result<()> {
    // Mount is idempotent in the client. Check every partition so a partial
    // unmount failure can restore the card without disturbing mounted volumes.
    let mut failures = Vec::new();
    for path in volumes(device)? {
        if let Err(e) = client.mount(&path) {
            failures.push(format!("{}: {e:#}", path.display()));
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

fn unmount_volumes(device: &Device, client: &mut impl MountClient) -> Result<()> {
    if device.mounts.is_empty() {
        return Ok(());
    }
    for path in volumes(device)? {
        client.unmount(&path)?;
    }
    Ok(())
}

/// Called at most once per USB attachment. A busy card or an automount race
/// cancels the initial check; the transport independently rechecks mount state.
#[derive(Debug)]
struct StorageRemounted;
impl std::fmt::Display for StorageRemounted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Camera storage mounted automatically before the initial check")
    }
}
impl std::error::Error for StorageRemounted {}
pub fn mounted_again(error: &anyhow::Error) -> bool {
    error.is::<StorageRemounted>()
}
pub fn prepare(device: &Device) -> Result<Device> {
    let attachment = camera::connection_identity(device).context("Camera disconnected")?;
    let current = camera::discover_checked()?
        .into_iter()
        .find(|d| camera::connection_identity(d).as_ref() == Some(&attachment))
        .context("Camera disconnected")?;
    if !current.mounts.is_empty() {
        let bus = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>)?;
        unmount_volumes(&current, &mut UDisks(bus))?;
    }
    let current = camera::discover_checked()?
        .into_iter()
        .find(|d| camera::connection_identity(d).as_ref() == Some(&attachment))
        .context("Camera disconnected")?;
    if !current.mounts.is_empty() {
        return Err(StorageRemounted.into());
    }
    Ok(current)
}

pub fn release(device: &Device) -> Result<()> {
    if !device.model.supports_gps() {
        let usb = device
            .sys_path
            .ancestors()
            .find(|p| p.join("idProduct").is_file())
            .context("Camera disconnected")?;
        ensure!(
            fs::read_to_string(usb.join("idProduct"))?.trim() == "0124",
            "Camera control mode is still active; reconnect USB to restore storage"
        );
    }
    let attachment = camera::connection_identity(device).context("Camera disconnected")?;
    let bus = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>)?;
    let mut client = UDisks(bus);
    let started = Instant::now();
    loop {
        ensure!(
            camera::connection_identity(device).as_ref() == Some(&attachment),
            "Camera disconnected"
        );
        let current = camera::discover()
            .into_iter()
            .find(|d| camera::connection_identity(d).as_ref() == Some(&attachment));
        let error = if let Some(current) = current {
            match mount_volumes(&current, &mut client) {
                Ok(()) => return Ok(()),
                Err(e) => e,
            }
        } else {
            anyhow::anyhow!("Waiting for camera storage to reappear")
        };
        if started.elapsed() >= Duration::from_secs(8) {
            return Err(error);
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// Service cleanup runs even when startup is disabled or the GUI fails. Avoid
/// racing an existing dashboard instance which owns the same initial update.
pub fn fallback() -> Result<()> {
    let config = backend::Config {
        project: PathBuf::new(),
        state_dir: crate::default_state_dir(),
        demo: false,
        monitor_only: true,
    };
    let _lock = match backend::lock_instance(&config) {
        Ok(lock) => lock,
        Err(e) if e.to_string().contains("already running") => return Ok(()),
        Err(e) => return Err(e),
    };
    let started = Instant::now();
    loop {
        let devices = camera::discover();
        if !devices.is_empty() {
            let mut failures = Vec::new();
            for device in devices {
                if let Err(e) = release(&device) {
                    failures.push(format!("{e:#}"));
                }
            }
            if !failures.is_empty() {
                bail!("{}", failures.join("; "));
            }
            return Ok(());
        }
        if started.elapsed() >= Duration::from_secs(2) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        calls: Vec<PathBuf>,
        unmounts: Vec<PathBuf>,
        mounted: std::collections::HashSet<PathBuf>,
        busy_volume: Option<PathBuf>,
        fail: bool,
    }
    impl MountClient for Fake {
        fn unmount(&mut self, path: &Path) -> Result<()> {
            self.unmounts.push(path.into());
            ensure!(
                !self.fail && self.busy_volume.as_deref() != Some(path),
                "Card is busy"
            );
            self.mounted.remove(path);
            Ok(())
        }
        fn mount(&mut self, path: &Path) -> Result<()> {
            if self.mounted.contains(path) {
                return Ok(());
            }
            self.calls.push(path.into());
            ensure!(!self.fail, "Injected mount failure");
            self.mounted.insert(path.into());
            Ok(())
        }
    }
    #[test]
    fn initial_unmount_handles_partitions_and_stops_at_a_busy_card() {
        let temp = crate::test_support::Temp::new();
        let mut device = Device {
            path: "/dev/sdb".into(),
            sys_path: temp.path().into(),
            mounts: vec!["/media/camera".into()],
            ..Default::default()
        };
        for name in ["sdb1", "sdb2"] {
            fs::create_dir(temp.path().join(name)).unwrap();
            fs::write(temp.path().join(name).join("partition"), "1").unwrap();
        }
        let mut client = Fake::default();
        unmount_volumes(&device, &mut client).unwrap();
        assert_eq!(
            client.unmounts,
            [PathBuf::from("/dev/sdb1"), PathBuf::from("/dev/sdb2")]
        );
        device.mounts.clear();
        mount_volumes(&device, &mut client).unwrap();
        assert_eq!(client.calls, client.unmounts);
        client.unmounts.clear();
        unmount_volumes(&device, &mut client).unwrap();
        assert!(client.unmounts.is_empty());
        device.mounts.push("/media/camera".into());
        client.fail = true;
        assert!(unmount_volumes(&device, &mut client).is_err());
        assert_eq!(client.unmounts, [PathBuf::from("/dev/sdb1")]);
    }
    #[test]
    fn partial_unmount_failure_restores_only_the_unmounted_partition() {
        let temp = crate::test_support::Temp::new();
        let device = Device {
            path: "/dev/sdb".into(),
            sys_path: temp.path().into(),
            mounts: vec!["/media/card1".into(), "/media/card2".into()],
            ..Default::default()
        };
        for name in ["sdb1", "sdb2"] {
            fs::create_dir(temp.path().join(name)).unwrap();
            fs::write(temp.path().join(name).join("partition"), "1").unwrap();
        }
        let mut client = Fake {
            mounted: [PathBuf::from("/dev/sdb1"), PathBuf::from("/dev/sdb2")].into(),
            busy_volume: Some("/dev/sdb2".into()),
            ..Default::default()
        };
        assert!(unmount_volumes(&device, &mut client).is_err());
        mount_volumes(&device, &mut client).unwrap();
        assert_eq!(client.calls, [PathBuf::from("/dev/sdb1")]);
        assert_eq!(client.mounted.len(), 2);
    }
    #[test]
    fn release_mounts_partitions_and_leaves_browsable_storage_alone() {
        let temp = crate::test_support::Temp::new();
        let mut device = Device {
            path: "/dev/sdb".into(),
            sys_path: temp.path().into(),
            ..Default::default()
        };
        let mut client = Fake::default();
        mount_volumes(&device, &mut client).unwrap();
        assert_eq!(client.calls, [PathBuf::from("/dev/sdb")]);
        fs::create_dir(temp.path().join("sdb1")).unwrap();
        fs::write(temp.path().join("sdb1/partition"), "1").unwrap();
        client.calls.clear();
        mount_volumes(&device, &mut client).unwrap();
        assert_eq!(client.calls, [PathBuf::from("/dev/sdb1")]);
        device.mounts.push("/media/camera".into());
        client.calls.clear();
        mount_volumes(&device, &mut client).unwrap();
        assert!(client.calls.is_empty());
    }
    #[test]
    fn mount_failures_are_reported_and_device_names_are_encoded() {
        let temp = crate::test_support::Temp::new();
        let device = Device {
            path: "/dev/sdb".into(),
            sys_path: temp.path().into(),
            ..Default::default()
        };
        assert!(
            mount_volumes(
                &device,
                &mut Fake {
                    fail: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert_eq!(
            object_path(Path::new("/dev/dm-0")).unwrap(),
            "/org/freedesktop/UDisks2/block_devices/dm_2d0"
        );
    }
}
