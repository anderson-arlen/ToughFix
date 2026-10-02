// SPDX-License-Identifier: MIT
//! Verified Tough-8010 USB mode switch: 0124 storage -> 0123 telemetry -> 0124.
//! Reconstructed from Olympus's communication library; no vendor binary used.
use super::*;
use std::time::Instant;

pub(super) fn mode_cdb(mode: u8) -> [u8; 12] {
    let mut cdb = [0; 12];
    cdb[0] = 0xdf;
    cdb[9] = mode;
    cdb
}

fn usb_path(device: &Device) -> Result<&Path> {
    device
        .sys_path
        .ancestors()
        .find(|p| p.join("idVendor").is_file())
        .context("Camera USB interface disappeared")
}

fn pid(device: &Device) -> Result<String> {
    Ok(fs::read_to_string(usb_path(device)?.join("idProduct"))?
        .trim()
        .to_owned())
}

fn switch(device: &Device, mode: u8) -> Result<Device> {
    ensure!(
        matches!(device.model, CameraModel::Tough8010) && mode <= 1,
        "Mode switching is restricted to the Tough-8010"
    );
    ensure!(
        device.serial_key.is_some(),
        "A USB serial is required for automatic mode switching"
    );
    let expected_from = if mode == 0 { "0124" } else { "0123" };
    let expected_to = if mode == 0 { "0123" } else { "0124" };
    ensure!(
        pid(device)? == expected_from,
        "Unexpected camera mode; reconnect USB"
    );
    let attachment = connection_identity(device).context("Camera disconnected")?;
    let location = usb_path(device)?.to_owned();
    let mut session = Session::new(LinuxTransport::open(device, false)?);
    session.inquiry_for(&device.model)?;
    session
        .io
        .transfer(&mode_cdb(mode), 0, None, 5000)
        .context(ReconnectRequired)?;
    drop(session); // The old SCSI endpoint becomes invalid during re-enumeration.
    wait_for_mode(device, &location, &attachment, expected_to).context(ReconnectRequired)
}

fn same_camera(original: &Device, candidate: &Device, location: &Path) -> bool {
    matches!(candidate.model, CameraModel::Tough8010)
        && candidate.serial_key == original.serial_key
        && candidate.connection_key == original.connection_key
        && usb_path(candidate).is_ok_and(|p| p == location)
}

fn wait_for_mode(
    original: &Device,
    location: &Path,
    old_attachment: &str,
    target: &str,
) -> Result<Device> {
    let started = Instant::now();
    let mut stable = None;
    loop {
        let devices = discover_checked()?;
        let current = devices.into_iter().find(|d| {
            same_camera(original, d, location)
                && pid(d).is_ok_and(|p| p == target)
                && connection_identity(d).is_some_and(|id| id != old_attachment)
                && d.capacity > 0
                && d.sys_path.join("device/scsi_generic").is_dir()
        });
        if let Some(current) = current {
            let key = connection_identity(&current);
            if key == stable {
                // Allow udev's active-seat ACL application to finish before opening SG.
                thread::sleep(Duration::from_millis(500));
                return Ok(current);
            }
            stable = key;
        } else {
            stable = None;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(12),
            "Camera did not return in USB mode {target}; reconnect USB"
        );
        thread::sleep(Duration::from_millis(250));
    }
}

pub(super) fn probe_8010(device: &Device) -> Result<CameraInfo> {
    round_trip(device, switch, |current| {
        let mut session = Session::new(LinuxTransport::open(current, false)?);
        probe_session(&mut session, current)
    })
}

fn round_trip(
    device: &Device,
    mut change: impl FnMut(&Device, u8) -> Result<Device>,
    read: impl FnOnce(&Device) -> Result<CameraInfo>,
) -> Result<CameraInfo> {
    let control = change(device, 0)?;
    let result = read(&control);
    if result.as_ref().is_err_and(requires_reconnect) {
        // Do not send another command after an uncertain PTP transaction.
        return result;
    }
    change(&control, 1)
        .context("Could not restore camera storage mode")
        .context(ReconnectRequired)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn battery_read_restores_storage_even_after_a_completed_query_rejection() {
        let device = Device {
            model: CameraModel::Tough8010,
            ..Default::default()
        };
        for failed in [false, true] {
            let calls = RefCell::new(Vec::new());
            let result = round_trip(
                &device,
                |d, mode| {
                    calls.borrow_mut().push(mode);
                    Ok(d.clone())
                },
                |_| {
                    calls.borrow_mut().push(9);
                    if failed {
                        bail!("Completed query rejected")
                    }
                    Ok(CameraInfo {
                        battery: Some(100),
                        ..Default::default()
                    })
                },
            );
            assert_eq!(*calls.borrow(), [0, 9, 1]);
            assert_eq!(result.is_err(), failed);
        }
    }

    #[test]
    fn failed_return_and_incomplete_session_require_reconnect() {
        let device = Device {
            model: CameraModel::Tough8010,
            ..Default::default()
        };
        let calls = RefCell::new(Vec::new());
        let error = round_trip(
            &device,
            |d, mode| {
                calls.borrow_mut().push(mode);
                Ok(d.clone())
            },
            |_| Err(ReconnectRequired.into()),
        )
        .unwrap_err();
        assert!(requires_reconnect(&error));
        assert_eq!(*calls.borrow(), [0]);
        let error = round_trip(
            &device,
            |d, mode| {
                if mode == 1 {
                    bail!("Return rejected")
                }
                Ok(d.clone())
            },
            |_| Ok(CameraInfo::default()),
        )
        .unwrap_err();
        assert!(requires_reconnect(&error));
    }
}
