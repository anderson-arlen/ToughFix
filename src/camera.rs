// SPDX-License-Identifier: MIT
//! Linux SG_IO and the recovered Olympus PTP tunnel. No normal sector writes.
use crate::model::{CameraInfo, CameraModel, Device, Phase, Storage};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

const MAX_CONTAINER: usize = 65_548;
const CHUNK: usize = 61_440;
const REQUIRED: &[u16] = &[
    0x1001, 0x1002, 0x1003, 0x9126, 0x9127, 0x9128, 0x9129, 0x912a, 0x912b, 0x912c,
];
mod tough8010;
use tough8010::{mode_cdb, probe_8010};

#[derive(Debug)]
struct ReconnectRequired;

#[derive(Debug)]
struct StorageMounted;
impl std::fmt::Display for StorageMounted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Unmount camera storage before checking the camera or uploading GPS assistance")
    }
}
impl std::error::Error for StorageMounted {}
pub fn storage_in_use(error: &anyhow::Error) -> bool {
    error.is::<StorageMounted>()
}

impl std::fmt::Display for ReconnectRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Camera session is uncertain; reconnect before retrying")
    }
}

impl std::error::Error for ReconnectRequired {}

pub fn requires_reconnect(error: &anyhow::Error) -> bool {
    error.is::<ReconnectRequired>()
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn discover() -> Vec<Device> {
    discover_checked().unwrap_or_default()
}

pub(crate) fn discover_checked() -> Result<Vec<Device>> {
    let mounts = fs::read_to_string("/proc/self/mountinfo")
        .context("Cannot verify whether camera storage is mounted")?;
    Ok(discover_at(
        Path::new("/sys/class/block"),
        Path::new("/dev"),
        &mounts,
    ))
}

fn discover_at(root: &Path, devices: &Path, mounts: &str) -> Vec<Device> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("partition").exists() {
            continue;
        }
        let Ok(sys) = fs::canonicalize(&path) else {
            continue;
        };
        let get = |p: PathBuf| fs::read_to_string(p).unwrap_or_default().trim().to_owned();
        if get(sys.join("device/vendor")) != "OLYMPUS" {
            continue;
        }
        let model = match get(sys.join("device/model")).as_str() {
            "TG-1" => CameraModel::Tg1,
            "StylusTough-8010" => CameraModel::Tough8010,
            _ => continue,
        };
        let Some(usb) = sys.ancestors().find(|p| p.join("idVendor").exists()) else {
            continue;
        };
        if get(usb.join("idVendor")) != "07b4" || !model.accepts_pid(&get(usb.join("idProduct"))) {
            continue;
        }
        let serial = get(usb.join("serial"));
        let key = hash(if serial.is_empty() {
            usb.as_os_str().as_encoded_bytes()
        } else {
            serial.as_bytes()
        });
        let number = get(sys.join("dev"));
        // Partitions share the disk name prefix but have their own device numbers.
        let mut numbers = vec![number];
        if let Ok(children) = fs::read_dir(&sys) {
            for child in children.flatten() {
                if child.path().join("partition").exists() {
                    numbers.push(get(child.path().join("dev")));
                }
            }
        }
        let mounted: Vec<String> = mounts
            .lines()
            .filter_map(|line| {
                let parts: Vec<_> = line.split_whitespace().collect();
                (parts.len() > 5 && numbers.iter().any(|n| n == parts[2])).then(|| {
                    parts[4]
                        .replace("\\040", " ")
                        .replace("\\011", "\t")
                        .replace("\\012", "\n")
                        .replace("\\134", "\\")
                })
            })
            .collect();
        found.push(Device {
            model,
            path: devices.join(entry.file_name()),
            sys_path: sys.clone(),
            connection_key: key,
            serial_key: (!serial.is_empty()).then(|| hash(serial.as_bytes())),
            capacity: get(sys.join("size"))
                .parse::<u64>()
                .unwrap_or(0)
                .saturating_mul(512),
            mounted_storage: mounted
                .iter()
                .filter_map(|p| filesystem_storage(Path::new(p)))
                .collect(),
            mounts: mounted,
        });
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Passive filesystem statistics; this never opens the SCSI/PTP interface.
#[allow(clippy::unnecessary_cast)] // statvfs integer widths vary by Linux target.
fn filesystem_storage(path: &Path) -> Option<Storage> {
    let name = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the path is NUL terminated and the output buffer has the required
    // layout. Only a successful statvfs call permits reading the initialized data.
    if unsafe { libc::statvfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs succeeded and initialized the whole output structure.
    let stats = unsafe { stats.assume_init() };
    let block_size = stats.f_frsize as u64;
    Some(Storage {
        label: "Mounted camera storage".into(),
        capacity: (stats.f_blocks as u64).saturating_mul(block_size),
        free: (stats.f_bavail as u64).saturating_mul(block_size),
        writable: stats.f_flag & libc::ST_RDONLY == 0,
    })
}

/// Identify the USB attachment, rather than a SCSI disk which can disappear
/// briefly when a vendor session changes modes. Also works with the last known
/// disk path while that disk is absent, as its USB ancestor can remain present.
pub fn connection_identity(device: &Device) -> Option<String> {
    let usb = device
        .sys_path
        .ancestors()
        .find(|p| p.join("idVendor").is_file())?;
    let read = |name| {
        fs::read_to_string(usb.join(name))
            .ok()
            .map(|s| s.trim().to_owned())
    };
    if read("idVendor")? != "07b4" || !device.model.accepts_pid(&read("idProduct")?) {
        return None;
    }
    Some(format!(
        "{}:{}:{}",
        device.connection_key,
        read("busnum")?,
        read("devnum")?
    ))
}

fn ensure_storage_unmounted(device: &Device) -> Result<()> {
    if !device.mounts.is_empty() {
        return Err(StorageMounted.into());
    }
    Ok(())
}

pub trait Transport {
    fn transfer(
        &mut self,
        cdb: &[u8],
        size: usize,
        outgoing: Option<&[u8]>,
        timeout: u32,
    ) -> Result<Vec<u8>>;
}

#[repr(C)]
#[derive(Default)]
struct SgIoHdr {
    interface_id: libc::c_int,
    dxfer_direction: libc::c_int,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: libc::c_int,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: libc::c_int,
    duration: u32,
    info: u32,
}

pub struct LinuxTransport {
    file: File,
}
impl LinuxTransport {
    pub fn open(device: &Device, _write: bool) -> Result<Self> {
        ensure_storage_unmounted(device)?;
        let identity = discover_checked()?
            .into_iter()
            .find(|d| d.path == device.path)
            .context("Camera disconnected")?;
        // Recheck the current kernel mount table rather than trusting a scan
        // taken earlier by the worker. No vendor command has been issued yet.
        ensure_storage_unmounted(&identity)?;
        ensure!(
            identity.sys_path == device.sys_path
                && identity.connection_key == device.connection_key,
            "Device identity changed"
        );
        // The block-device SG_IO path filters vendor CDBs for unprivileged users.
        // A verified SCSI generic character endpoint opened O_RDWR supports them
        // without giving the GUI CAP_SYS_RAWIO or running the GUI as root.
        let generic_dir = device.sys_path.join("device/scsi_generic");
        let endpoint=fs::read_dir(&generic_dir).context("Camera SCSI generic interface unavailable; load the sg kernel module and reconnect")?
            .filter_map(|entry|entry.ok()).next().context("Camera SCSI generic endpoint is missing")?;
        let generic_sys = fs::canonicalize(endpoint.path())?;
        let scsi_sys = fs::canonicalize(device.sys_path.join("device"))?;
        ensure!(
            generic_sys.parent().and_then(Path::parent) == Some(scsi_sys.as_path()),
            "SCSI generic endpoint does not belong to this camera"
        );
        let endpoint_path = Path::new("/dev").join(endpoint.file_name());
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&endpoint_path)
            .context(
                "Cannot access camera SCSI interface; install the supplied camera permission rule",
            )?;
        let original = fs::metadata(&endpoint_path)?;
        let opened = file.metadata()?;
        ensure!(
            opened.file_type().is_char_device() && opened.rdev() == original.rdev(),
            "Expected the verified camera SCSI character device"
        );
        let sys = fs::canonicalize(format!(
            "/sys/dev/char/{}:{}",
            libc::major(opened.rdev()),
            libc::minor(opened.rdev())
        ))?;
        ensure!(sys == generic_sys, "Camera device changed during open");
        // SAFETY: file owns a live descriptor; flock takes only an integer argument.
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "Camera is already in use by another updater"
        );
        Ok(Self { file })
    }
}
impl Transport for LinuxTransport {
    fn transfer(
        &mut self,
        cdb: &[u8],
        size: usize,
        outgoing: Option<&[u8]>,
        timeout: u32,
    ) -> Result<Vec<u8>> {
        ensure!(
            size <= MAX_CONTAINER
                && cdb.len() <= 16
                && (size > 0 || (outgoing.is_none() && (cdb == mode_cdb(0) || cdb == mode_cdb(1)))),
            "Invalid SCSI transfer size"
        );
        ensure!(
            outgoing.is_none_or(|d| d.len() == size),
            "Invalid outgoing transfer length"
        );
        let mut data = outgoing.map_or_else(|| vec![0; size], |d| d.to_vec());
        let mut command = cdb.to_vec();
        let mut sense = [0u8; 64];
        let mut header = SgIoHdr {
            interface_id: b'S' as i32,
            dxfer_direction: if size == 0 {
                -1
            } else if outgoing.is_some() {
                -2
            } else {
                -3
            },
            cmd_len: cdb.len() as u8,
            mx_sb_len: 64,
            dxfer_len: size as u32,
            dxferp: data.as_mut_ptr().cast(),
            cmdp: command.as_mut_ptr(),
            sbp: sense.as_mut_ptr(),
            timeout,
            ..Default::default()
        };
        // SAFETY: SG_IO receives a correctly laid out header; its buffers stay alive
        // and exclusively borrowed for the entire synchronous ioctl call.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), 0x2285 as libc::c_ulong, &mut header) } < 0 {
            return Err(std::io::Error::last_os_error()).context("Camera SG_IO operation failed");
        }
        ensure!(
            header.status == 0 && header.host_status == 0 && header.driver_status == 0,
            "Camera SCSI failure (status {}, host {}, driver {})",
            header.status,
            header.host_status,
            header.driver_status
        );
        ensure!(
            header.resid >= 0 && header.resid as usize <= size,
            "Invalid SCSI residual length"
        );
        ensure!(
            outgoing.is_none() || header.resid == 0,
            "Short outgoing transfer; reconnect before retrying"
        );
        data.truncate(size - header.resid as usize);
        Ok(data)
    }
}

pub struct Session<T: Transport> {
    io: T,
    transaction: u32,
    pub idle: bool,
    pub timeout: u32,
    pub validated: bool,
    pub committed: bool,
}
impl<T: Transport> Session<T> {
    pub fn new(io: T) -> Self {
        Self {
            io,
            transaction: 1,
            idle: true,
            timeout: 10_000,
            validated: false,
            committed: false,
        }
    }
    fn tunnel(&mut self, opcode: u8, size: usize, outgoing: Option<&[u8]>) -> Result<Vec<u8>> {
        ensure!(matches!(opcode, 0xc0..=0xc4), "Disallowed transport opcode");
        ensure!(
            outgoing.is_some() == matches!(opcode, 0xc0 | 0xc1),
            "Invalid transport direction"
        );
        let mut cdb = [0u8; 16];
        cdb[0] = opcode;
        cdb[6..10].copy_from_slice(&(size as u32).to_be_bytes());
        self.io.transfer(&cdb, size, outgoing, self.timeout)
    }
    pub fn inquiry(&mut self) -> Result<()> {
        self.inquiry_for(&CameraModel::Tg1)
    }
    fn inquiry_for(&mut self, model: &CameraModel) -> Result<()> {
        let raw = self
            .io
            .transfer(&[0x12, 0, 0, 0, 96, 0], 96, None, self.timeout)?;
        ensure!(
            raw.len() >= 36
                && raw[8..16].trim_ascii() == b"OLYMPUS"
                && raw[16..32].trim_ascii() == model.scsi_name().as_bytes(),
            "SCSI identity is not {}",
            model.name()
        );
        Ok(())
    }
    pub fn operation(
        &mut self,
        code: u16,
        params: &[u32],
        receive: bool,
        payload: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        if !self.idle {
            return Err(ReconnectRequired.into());
        }
        let result = self.operation_inner(code, params, receive, payload);
        result.map_err(|error| {
            if self.idle {
                error
            } else {
                error.context(ReconnectRequired)
            }
        })
    }

    fn operation_inner(
        &mut self,
        code: u16,
        params: &[u32],
        receive: bool,
        payload: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        let expected = match code {
            0x1001 | 0x1003 | 0x1004 | 0x9126 | 0x9127 | 0x912a | 0x912b | 0x912c => 0,
            0x1002 | 0x1005 | 0x1015 | 0x9128 => 1,
            0x9129 => 2,
            _ => bail!("Disallowed camera command"),
        };
        ensure!(
            params.len() == expected,
            "Incorrect camera command parameters"
        );
        ensure!(
            receive
                == matches!(
                    code,
                    0x1001 | 0x1004 | 0x1005 | 0x1015 | 0x9126 | 0x9127 | 0x912b
                ),
            "Incorrect camera data phase"
        );
        ensure!(
            payload.is_some() == (code == 0x9129),
            "Outgoing data only allowed for assistance chunks"
        );
        if code == 0x1002 {
            ensure!(params == [1], "Only session 1 is permitted");
        }
        if code == 0x1015 {
            ensure!(params == [0x5001], "Only battery telemetry is queried");
        }
        if code == 0x9128 {
            ensure!(params[0] == 130720, "Expected four-week CEP container");
            self.validated = false;
            self.committed = false;
        }
        if code == 0x9129 {
            let data = payload.unwrap();
            ensure!(
                !data.is_empty()
                    && data.len() <= CHUNK
                    && params[1] as usize == data.len()
                    && params[0] as usize + data.len() <= 130720,
                "Invalid assistance chunk"
            );
        }
        if code == 0x912c {
            ensure!(
                self.validated && !self.committed,
                "Commit requires staged validation and cannot be repeated"
            );
        }
        let txn = self.transaction;
        let mut command = container(
            1,
            code,
            txn,
            &params
                .iter()
                .flat_map(|p| p.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        command.resize(64, 0);
        self.idle = false;
        self.tunnel(0xc0, 64, Some(&command))?;
        if let Some(data) = payload {
            let raw = container(2, code, txn, data);
            self.tunnel(0xc1, raw.len(), Some(&raw))?;
        }
        let data = if receive {
            let size = self.tunnel(0xc4, 64, None)?;
            ensure!(size.len() >= 16, "Truncated incoming size reply");
            let length = u32::from_le_bytes(size[12..16].try_into()?) as usize;
            ensure!(
                (12..=MAX_CONTAINER).contains(&length),
                "Invalid incoming size"
            );
            let raw = self.tunnel(0xc2, length, None)?;
            parse_container(&raw, 2, Some(code), txn)?.1.to_vec()
        } else {
            Vec::new()
        };
        let raw = self.tunnel(0xc3, 64, None)?;
        let (status, _) = parse_container(&raw, 3, None, txn)?;
        self.transaction += 1;
        self.idle = true;
        ensure!(
            status == 0x2001,
            "Camera rejected 0x{code:04x}: 0x{status:04x}"
        );
        if code == 0x912c {
            self.committed = true;
        }
        Ok(data)
    }
    pub fn upload(
        &mut self,
        data: &[u8],
        limit: u32,
        mut progress: impl FnMut(Phase),
        mut committed: impl FnMut() -> Result<()>,
        sleep: impl Fn(Duration),
    ) -> Result<()> {
        ensure!(
            data.len() == 130720 && (1..=131072).contains(&limit),
            "Invalid archive or camera transfer limit"
        );
        self.operation(0x9128, &[data.len() as u32], false, None)?;
        let chunk = (limit as usize).min(CHUNK);
        for (i, part) in data.chunks(chunk).enumerate() {
            self.operation(
                0x9129,
                &[(i * chunk) as u32, part.len() as u32],
                false,
                Some(part),
            )?;
            progress(Phase::Transferring {
                sent: (i * chunk + part.len()),
                total: data.len(),
            });
        }
        self.operation(0x912a, &[], false, None)?;
        progress(Phase::Validating);
        for _ in 0..180 {
            sleep(Duration::from_secs(1));
            let raw = self.operation(0x912b, &[], true, None)?;
            let state = word(&raw)?;
            match state {
                1 => continue,
                2 => {
                    self.validated = true;
                    progress(Phase::Committing);
                    self.timeout = 120000;
                    let result = self.operation(0x912c, &[], false, None);
                    self.timeout = 10000;
                    result?;
                    committed()?;
                    return Ok(());
                }
                _ => bail!("Unexpected validation state {state}; reconnect before retrying"),
            }
        }
        bail!("Camera validation timed out; reconnect before retrying")
    }
}

pub fn container(kind: u16, code: u16, txn: u32, body: &[u8]) -> Vec<u8> {
    let mut raw = Vec::with_capacity(12 + body.len());
    raw.extend(((12 + body.len()) as u32).to_le_bytes());
    raw.extend(kind.to_le_bytes());
    raw.extend(code.to_le_bytes());
    raw.extend(txn.to_le_bytes());
    raw.extend(body);
    raw
}
pub fn parse_container(raw: &[u8], kind: u16, code: Option<u16>, txn: u32) -> Result<(u16, &[u8])> {
    ensure!(raw.len() >= 12, "Truncated PTP container");
    let length = u32::from_le_bytes(raw[0..4].try_into()?) as usize;
    let actual = u16::from_le_bytes(raw[6..8].try_into()?);
    ensure!(
        (12..=raw.len()).contains(&length)
            && u16::from_le_bytes(raw[4..6].try_into()?) == kind
            && u32::from_le_bytes(raw[8..12].try_into()?) == txn
            && code.is_none_or(|c| c == actual),
        "PTP framing or transaction mismatch"
    );
    Ok((actual, &raw[12..length]))
}
fn word(raw: &[u8]) -> Result<u32> {
    ensure!(raw.len() == 4, "Expected a four-byte response");
    Ok(u32::from_le_bytes(raw.try_into()?))
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("Dataset overflow")?;
        ensure!(end <= self.data.len(), "Truncated PTP dataset");
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into()?))
    }
    fn string(&mut self) -> Result<String> {
        let n = self.take(1)?[0] as usize;
        let raw = self.take(n * 2)?;
        let chars: Vec<_> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Ok(String::from_utf16(&chars)?
            .trim_end_matches('\0')
            .to_string())
    }
    fn array(&mut self) -> Result<Vec<u16>> {
        let n = self.u32()? as usize;
        ensure!(n <= 32768, "Excessive PTP array");
        (0..n).map(|_| self.u16()).collect()
    }
}
#[derive(Debug)]
struct Identity {
    firmware: String,
    key: String,
    operations: Vec<u16>,
    properties: Vec<u16>,
}
fn identity(raw: &[u8], fallback: &str, expected: &CameraModel) -> Result<Identity> {
    let mut r = Reader { data: raw, pos: 0 };
    r.take(8)?;
    r.string()?;
    r.u16()?;
    let operations = r.array()?;
    r.array()?;
    let properties = r.array()?;
    r.array()?;
    r.array()?;
    let manufacturer = r.string()?;
    let model = r.string()?;
    let firmware = r.string()?;
    let serial = r.string()?;
    ensure!(
        manufacturer == "OLYMPUS" && model == expected.scsi_name(),
        "PTP identity is not {}",
        expected.name()
    );
    ensure!(
        (if expected.supports_gps() {
            REQUIRED
        } else {
            &[0x1001, 0x1002, 0x1003]
        })
        .iter()
        .all(|o| operations.contains(o)),
        "Required camera operations not advertised"
    );
    Ok(Identity {
        firmware,
        key: if serial.is_empty() {
            fallback.to_owned()
        } else {
            hash(serial.as_bytes())
        },
        operations,
        properties,
    })
}
pub fn capture_key(raw: &[u8]) -> Result<String> {
    let key = identity(raw, "", &CameraModel::Tg1)?.key;
    ensure!(!key.is_empty(), "No serial identity in capture");
    Ok(key)
}
fn storages<T: Transport>(session: &mut Session<T>) -> Result<Vec<Storage>> {
    let ids = session.operation(0x1004, &[], true, None)?;
    let mut reader = Reader { data: &ids, pos: 0 };
    let count = reader.u32()?;
    ensure!(
        count <= 32 && ids.len() == 4 + count as usize * 4,
        "Invalid storage ID array"
    );
    let mut result = Vec::new();
    for _ in 0..count {
        let id = reader.u32()?;
        let info = session.operation(0x1005, &[id], true, None)?;
        let mut r = Reader {
            data: &info,
            pos: 0,
        };
        r.u16()?;
        r.u16()?;
        let access = r.u16()?;
        let capacity = r.u64()?;
        let free = r.u64()?;
        r.u32()?;
        let description = r.string()?;
        let label = r.string()?;
        ensure!(free <= capacity, "Invalid storage free-space report");
        result.push(Storage {
            label: if label.is_empty() {
                if description.is_empty() {
                    "Camera storage".into()
                } else {
                    description
                }
            } else {
                label
            },
            capacity,
            free,
            writable: access == 0,
        });
    }
    Ok(result)
}
fn info<T: Transport>(session: &mut Session<T>, device: &Device) -> Result<CameraInfo> {
    let id = identity(
        &session.operation(0x1001, &[], true, None)?,
        &device.connection_key,
        &device.model,
    )?;
    let battery = if id.operations.contains(&0x1015) && id.properties.contains(&0x5001) {
        match session.operation(0x1015, &[0x5001], true, None) {
            Ok(raw) if raw.len() == 1 && raw[0] <= 100 => Some(raw[0]),
            Ok(_) => None,
            Err(_) if session.idle => None,
            Err(e) => return Err(e),
        }
    } else {
        None
    };
    let storage = if id.operations.contains(&0x1004) && id.operations.contains(&0x1005) {
        storages(session)?
    } else {
        Vec::new()
    };
    // Finish identity, battery and SD card checks before issuing any
    // GPS-specific commands. A failed general check cannot reach an upload.
    let (gps_chip, transfer_limit) = if device.model.supports_gps() {
        let chip = word(&session.operation(0x9126, &[], true, None)?)?;
        let limit = word(&session.operation(0x9127, &[], true, None)?)?;
        ensure!(
            chip == 1 && (1..=131072).contains(&limit),
            "Unexpected GPS chip or transfer limit"
        );
        (chip, limit)
    } else {
        (0, 0)
    };
    Ok(CameraInfo {
        key: id.key,
        firmware: id.firmware,
        battery,
        gps_chip,
        transfer_limit,
        storage,
        read_at: Some(Utc::now()),
    })
}

pub fn probe(device: &Device) -> Result<CameraInfo> {
    if matches!(device.model, CameraModel::Tough8010) {
        return probe_8010(device);
    }
    let mut session = Session::new(LinuxTransport::open(device, false)?);
    probe_session(&mut session, device)
}

fn probe_session<T: Transport>(session: &mut Session<T>, device: &Device) -> Result<CameraInfo> {
    session.inquiry_for(&device.model)?;
    session.operation(0x1002, &[1], false, None)?;
    let result = info(session, device);
    close_session(session, result)
}

fn close_session<T: Transport, R>(session: &mut Session<T>, result: Result<R>) -> Result<R> {
    if !session.idle {
        return Err(match result {
            Err(error) => error.context(ReconnectRequired),
            Ok(_) => ReconnectRequired.into(),
        });
    }
    let closed = session
        .operation(0x1003, &[], false, None)
        .map_err(|error| error.context(ReconnectRequired));
    match (result, closed) {
        (Ok(value), Ok(_)) => Ok(value),
        (_, Err(e)) | (Err(e), _) => Err(e),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum UploadOutcome {
    Committed,
    AlreadyCurrent,
}

pub fn upload(
    device: &Device,
    data: &[u8],
    progress: impl FnMut(Phase),
    authorize: impl FnMut(&CameraInfo) -> Result<bool>,
    commit: impl FnMut(&CameraInfo) -> Result<()>,
) -> Result<UploadOutcome> {
    ensure!(
        device.model.supports_gps(),
        "This camera has no GPS receiver"
    );
    let mut session = Session::new(LinuxTransport::open(device, true)?);
    upload_session(
        &mut session,
        device,
        data,
        progress,
        authorize,
        commit,
        thread::sleep,
    )
}

fn upload_session<T: Transport>(
    session: &mut Session<T>,
    device: &Device,
    data: &[u8],
    mut progress: impl FnMut(Phase),
    mut authorize: impl FnMut(&CameraInfo) -> Result<bool>,
    mut commit: impl FnMut(&CameraInfo) -> Result<()>,
    sleep: impl Fn(Duration),
) -> Result<UploadOutcome> {
    ensure!(
        device.model.supports_gps(),
        "This camera has no GPS receiver"
    );
    progress(Phase::Checking);
    session.inquiry()?;
    session.operation(0x1002, &[1], false, None)?;
    let result = (|| {
        let camera = info(session, device)?;
        if !authorize(&camera)? {
            return Ok(UploadOutcome::AlreadyCurrent);
        }
        progress(Phase::Transferring {
            sent: 0,
            total: data.len(),
        });
        session.upload(
            data,
            camera.transfer_limit,
            &mut progress,
            || commit(&camera),
            sleep,
        )?;
        Ok(UploadOutcome::Committed)
    })();
    progress(Phase::Closing);
    close_session(session, result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn mounted_storage_blocks_probe_and_upload_before_opening_transport() {
        let mut device = fake_device();
        device.mounts = vec!["/media/camera".into()];
        let error = probe(&device).unwrap_err();
        assert!(error.to_string().contains("Unmount camera storage"));
        assert!(storage_in_use(&error.context("initial check")));
        let error = upload(
            &device,
            &[0; 130720],
            |_| {},
            |_| panic!("must not reach authorization"),
            |_| panic!("must not commit"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Unmount camera storage"));
        assert!(storage_in_use(&error));
        assert!(!requires_reconnect(&error));
    }
    #[test]
    fn usb_attachment_identity_survives_disk_rebinding_but_changes_on_reconnection() {
        let temp = crate::test_support::Temp::new();
        let usb = temp.path().join("usb-device");
        let mut device = fake_device();
        device.sys_path = usb.join("host1/target/disk/block/sda");
        fs::create_dir_all(&device.sys_path).unwrap();
        for (name, value) in [
            ("idVendor", "07b4"),
            ("idProduct", "012d"),
            ("busnum", "1"),
            ("devnum", "4"),
        ] {
            fs::write(usb.join(name), value).unwrap();
        }
        let original = connection_identity(&device).unwrap();
        fs::remove_dir_all(usb.join("host1")).unwrap();
        assert_eq!(connection_identity(&device).unwrap(), original);
        device.sys_path = usb.join("host2/target/disk/block/sdb");
        assert_eq!(connection_identity(&device).unwrap(), original);
        fs::write(usb.join("devnum"), "5").unwrap();
        assert_ne!(connection_identity(&device).unwrap(), original);
        fs::remove_file(usb.join("idVendor")).unwrap();
        assert!(connection_identity(&device).is_none());
    }
    #[test]
    fn mounted_volume_space_comes_from_the_filesystem_without_camera_queries() {
        let temp = crate::test_support::Temp::new();
        let storage = filesystem_storage(temp.path()).unwrap();
        assert!(storage.capacity > 0 && storage.free <= storage.capacity);
        assert!(storage.writable);
        assert!(filesystem_storage(&temp.path().join("absent")).is_none());
    }

    #[test]
    fn discovery_recognizes_partition_mounts_and_escaped_mount_paths() {
        use std::os::unix::fs::symlink;
        let temp = crate::test_support::Temp::new();
        let usb = temp.path().join("usb");
        let disk = usb.join("host/target/block/sda");
        let root = temp.path().join("class-block");
        fs::create_dir_all(disk.join("device")).unwrap();
        fs::create_dir_all(disk.join("sda1")).unwrap();
        fs::create_dir_all(&root).unwrap();
        for (path, value) in [
            (usb.join("idVendor"), "07b4"),
            (usb.join("idProduct"), "012d"),
            (disk.join("device/vendor"), "OLYMPUS"),
            (disk.join("device/model"), "TG-1"),
            (disk.join("dev"), "8:0"),
            (disk.join("sda1/dev"), "8:1"),
            (disk.join("sda1/partition"), "1"),
        ] {
            fs::write(path, value).unwrap();
        }
        symlink(&disk, root.join("sda")).unwrap();
        symlink(disk.join("sda1"), root.join("sda1")).unwrap();
        let found = discover_at(
            &root,
            Path::new("/dev"),
            r"22 1 8:1 / /media/camera\040card rw - vfat /dev/sda1 rw",
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mounts, ["/media/camera card"]);
        assert!(storage_in_use(
            &ensure_storage_unmounted(&found[0]).unwrap_err()
        ));
        fs::write(disk.join("device/model"), "StylusTough-8010").unwrap();
        for pid in ["0123", "0124"] {
            fs::write(usb.join("idProduct"), pid).unwrap();
            let found = discover_at(&root, Path::new("/dev"), "");
            assert_eq!(found.len(), 1);
            assert!(matches!(found[0].model, CameraModel::Tough8010));
        }
        fs::write(usb.join("idProduct"), "0125").unwrap();
        assert!(discover_at(&root, Path::new("/dev"), "").is_empty());
        fs::write(usb.join("idProduct"), "0124").unwrap();
        fs::write(disk.join("device/model"), "Unknown camera").unwrap();
        assert!(discover_at(&root, Path::new("/dev"), "").is_empty());
    }

    use super::*;
    #[derive(Default)]
    struct Fake {
        model: CameraModel,
        command: Option<(u16, u32)>,
        calls: Vec<(u8, u32)>,
        codes: Vec<u16>,
        state: u32,
        reject_commit: bool,
        fail_response_for: Option<u16>,
        reject_response_for: Option<u16>,
        device_info: Option<Vec<u8>>,
        body: Vec<u8>,
    }
    impl Fake {
        fn payload(&self) -> Vec<u8> {
            if let Some(dataset) = &self.device_info {
                match self.command.unwrap().0 {
                    0x1001 => return dataset.clone(),
                    0x9126 => return 1u32.to_le_bytes().to_vec(),
                    0x9127 => return 131072u32.to_le_bytes().to_vec(),
                    0x1015 => return vec![84],
                    0x1004 => return [1u32.to_le_bytes(), 1u32.to_le_bytes()].concat(),
                    0x1005 => {
                        let mut raw = vec![0; 6];
                        raw.extend(32_000_000_000u64.to_le_bytes());
                        raw.extend(28_000_000_000u64.to_le_bytes());
                        raw.extend(0u32.to_le_bytes());
                        raw.extend([0, 0]); // Empty description and label.
                        return raw;
                    }
                    _ => (),
                }
            }
            self.state.to_le_bytes().to_vec()
        }
    }
    impl Transport for Fake {
        fn transfer(
            &mut self,
            cdb: &[u8],
            _: usize,
            outgoing: Option<&[u8]>,
            timeout: u32,
        ) -> Result<Vec<u8>> {
            self.calls.push((cdb[0], timeout));
            match cdb[0] {
                0x12 => {
                    let mut raw = vec![0; 96];
                    raw[8..16].copy_from_slice(b"OLYMPUS ");
                    raw[16..32].fill(b' ');
                    let name = self.model.scsi_name().as_bytes();
                    raw[16..16 + name.len()].copy_from_slice(name);
                    Ok(raw)
                }
                0xc0 => {
                    let raw = outgoing.unwrap();
                    let code = u16::from_le_bytes(raw[6..8].try_into()?);
                    let txn = u32::from_le_bytes(raw[8..12].try_into()?);
                    self.command = Some((code, txn));
                    self.codes.push(code);
                    Ok(vec![])
                }
                0xc1 => {
                    self.body.extend(&outgoing.unwrap()[12..]);
                    Ok(vec![])
                }
                0xc4 => {
                    let mut raw = vec![0; 64];
                    raw[12..16]
                        .copy_from_slice(&((12 + self.payload().len()) as u32).to_le_bytes());
                    Ok(raw)
                }
                0xc2 => {
                    let (code, txn) = self.command.unwrap();
                    Ok(container(2, code, txn, &self.payload()))
                }
                0xc3 => {
                    let (code, txn) = self.command.unwrap();
                    if self.fail_response_for == Some(code) {
                        bail!("Injected transport failure")
                    }
                    Ok(container(
                        3,
                        if (code == 0x912c && self.reject_commit)
                            || self.reject_response_for == Some(code)
                        {
                            0x2002
                        } else {
                            0x2001
                        },
                        txn,
                        &[],
                    ))
                }
                _ => bail!("Unexpected fake call"),
            }
        }
    }
    fn fake_device() -> Device {
        Device {
            path: "/dev/fake-camera".into(),
            sys_path: "/sys/fake-camera".into(),
            connection_key: "fake-camera".into(),
            capacity: 0,
            mounts: vec![],
            ..Default::default()
        }
    }

    fn fake_identity() -> Vec<u8> {
        fake_identity_with_telemetry(false)
    }
    fn fake_identity_with_telemetry(telemetry: bool) -> Vec<u8> {
        fake_identity_for(&CameraModel::Tg1, telemetry)
    }
    fn fake_identity_for(model: &CameraModel, telemetry: bool) -> Vec<u8> {
        let mut raw = vec![0; 8];
        raw.push(0); // Empty extension description.
        raw.extend(0u16.to_le_bytes());
        let mut operations = if model.supports_gps() {
            REQUIRED.to_vec()
        } else {
            vec![0x1001, 0x1002, 0x1003]
        };
        if telemetry {
            operations.extend([0x1015, 0x1004, 0x1005]);
        }
        raw.extend((operations.len() as u32).to_le_bytes());
        for code in operations {
            raw.extend(code.to_le_bytes());
        }
        raw.extend(0u32.to_le_bytes()); // Events.
        raw.extend(u32::from(telemetry).to_le_bytes());
        if telemetry {
            raw.extend(0x5001u16.to_le_bytes());
        }
        for _ in 0..2 {
            raw.extend(0u32.to_le_bytes());
        }
        for value in ["OLYMPUS", model.scsi_name(), "1.00", "camera-a"] {
            let chars: Vec<_> = value.encode_utf16().collect();
            raw.push((chars.len() + 1) as u8);
            for c in chars {
                raw.extend(c.to_le_bytes());
            }
            raw.extend(0u16.to_le_bytes());
        }
        raw
    }

    #[test]
    fn non_gps_camera_reads_battery_and_storage_but_cannot_upload() {
        let device = Device {
            model: CameraModel::Tough8010,
            ..fake_device()
        };
        let mut session = Session::new(Fake {
            model: CameraModel::Tough8010,
            device_info: Some(fake_identity_for(&CameraModel::Tough8010, true)),
            ..Default::default()
        });
        let info = probe_session(&mut session, &device).unwrap();
        assert_eq!(info.battery, Some(84));
        assert_eq!((info.gps_chip, info.transfer_limit), (0, 0));
        assert_eq!(
            session.io.codes,
            [0x1002, 0x1001, 0x1015, 0x1004, 0x1005, 0x1003]
        );
        session.io.codes.clear();
        let error = upload_session(
            &mut session,
            &device,
            &[0; 130720],
            |_| {},
            |_| panic!("Cannot authorize non-GPS camera"),
            |_| panic!("Cannot commit to non-GPS camera"),
            |_| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("no GPS receiver"));
        assert!(session.io.codes.is_empty());
        assert!(upload(&device, &[0; 130720], |_| {}, |_| Ok(true), |_| Ok(())).is_err());
        assert!(
            identity(
                &fake_identity_for(&CameraModel::Tough8010, true),
                "",
                &CameraModel::Tg1
            )
            .is_err()
        );
    }

    #[test]
    fn initial_camera_information_and_upload_share_one_session() {
        let mut session = Session::new(Fake {
            device_info: Some(fake_identity_with_telemetry(true)),
            state: 2,
            ..Default::default()
        });
        let data = vec![42; 130720];
        let mut recorded = false;
        let result = upload_session(
            &mut session,
            &fake_device(),
            &data,
            |_| (),
            |info| {
                assert_eq!(info.firmware, "1.00");
                assert_eq!(info.gps_chip, 1);
                assert_eq!(info.battery, Some(84));
                assert_eq!(info.storage[0].capacity, 32_000_000_000);
                Ok(true)
            },
            |_| {
                recorded = true;
                Ok(())
            },
            |_| (),
        )
        .unwrap();
        assert_eq!(result, UploadOutcome::Committed);
        assert!(recorded && session.committed && session.idle);
        assert_eq!(
            session.io.codes,
            [
                0x1002, 0x1001, 0x1015, 0x1004, 0x1005, 0x9126, 0x9127, 0x9128, 0x9129, 0x9129,
                0x9129, 0x912a, 0x912b, 0x912c, 0x1003
            ]
        );
        assert_eq!(session.io.body, data);
    }

    #[test]
    fn failed_storage_check_closes_session_without_any_gps_commands() {
        let mut session = Session::new(Fake {
            device_info: Some(fake_identity_with_telemetry(true)),
            reject_response_for: Some(0x1005),
            ..Default::default()
        });
        let result = upload_session(
            &mut session,
            &fake_device(),
            &[0; 130720],
            |_| (),
            |_| panic!("failed checks cannot authorize GPS updates"),
            |_| panic!("failed checks cannot commit GPS data"),
            |_| (),
        );
        assert!(result.is_err());
        assert_eq!(
            session.io.codes,
            [0x1002, 0x1001, 0x1015, 0x1004, 0x1005, 0x1003]
        );
        assert!(session.idle && session.io.body.is_empty());
    }

    #[test]
    fn matching_camera_receipt_skips_transfer_and_closes_session() {
        let mut session = Session::new(Fake {
            device_info: Some(fake_identity()),
            ..Default::default()
        });
        let result = upload_session(
            &mut session,
            &fake_device(),
            &[0; 130720],
            |_| (),
            |_| Ok(false),
            |_| panic!("must not commit matching predictions"),
            |_| (),
        )
        .unwrap();
        assert_eq!(result, UploadOutcome::AlreadyCurrent);
        assert_eq!(session.io.codes, [0x1002, 0x1001, 0x9126, 0x9127, 0x1003]);
        assert!(session.io.body.is_empty() && session.idle && !session.committed);
    }

    #[test]
    fn telemetry_refresh_reads_health_and_storage_without_writing_assistance() {
        let mut session = Session::new(Fake {
            device_info: Some(fake_identity_with_telemetry(true)),
            ..Default::default()
        });
        let info = probe_session(&mut session, &fake_device()).unwrap();
        assert_eq!(info.battery, Some(84));
        assert_eq!(info.storage[0].capacity, 32_000_000_000);
        assert!(info.read_at.is_some());
        assert_eq!(
            session.io.codes,
            [
                0x1002, 0x1001, 0x1015, 0x1004, 0x1005, 0x9126, 0x9127, 0x1003
            ]
        );
        assert!(session.io.body.is_empty() && session.idle && !session.committed);
    }

    #[test]
    fn interrupted_probe_requires_reconnect_and_blocks_further_commands() {
        for code in [0x1002, 0x1001, 0x1003] {
            let mut session = Session::new(Fake {
                fail_response_for: Some(code),
                ..Default::default()
            });
            let error = probe_session(&mut session, &fake_device()).unwrap_err();
            // Classification survives added context and preserves the original cause.
            let error = error.context("Telemetry query failed");
            assert!(requires_reconnect(&error), "0x{code:04x}: {error:#}");
            assert!(format!("{error:#}").contains("Injected transport failure"));
            assert!(!session.idle);
            let calls = session.io.calls.len();
            assert!(requires_reconnect(
                &session.operation(0x1002, &[1], false, None).unwrap_err()
            ));
            assert_eq!(session.io.calls.len(), calls);
        }
    }

    #[test]
    fn rejected_probe_close_requires_reconnect_even_after_query_error() {
        let mut session = Session::new(Fake {
            reject_response_for: Some(0x1003),
            ..Default::default()
        });
        let error = probe_session(&mut session, &fake_device()).unwrap_err();
        assert!(requires_reconnect(&error));
        assert!(format!("{error:#}").contains("Camera rejected 0x1003"));
        assert!(session.idle);
    }

    #[test]
    fn completed_rejections_and_closed_probe_errors_can_be_retried() {
        let mut session = Session::new(Fake {
            reject_response_for: Some(0x1002),
            ..Default::default()
        });
        let error = probe_session(&mut session, &fake_device()).unwrap_err();
        assert!(!requires_reconnect(&error));
        assert!(session.idle);

        let mut session = Session::new(Fake::default());
        // Fake identity data is malformed, but its response and close are complete.
        let error = probe_session(&mut session, &fake_device()).unwrap_err();
        assert!(!requires_reconnect(&error));
        assert!(session.idle);
        assert_eq!(session.io.codes, [0x1002, 0x1001, 0x1003]);
    }
    #[test]
    fn commit_follows_validation_and_uses_long_timeout() {
        let data = vec![42; 130720];
        let mut session = Session::new(Fake {
            state: 2,
            ..Default::default()
        });
        let mut committed = false;
        session
            .upload(
                &data,
                131072,
                |_| (),
                || {
                    committed = true;
                    Ok(())
                },
                |_| (),
            )
            .unwrap();
        assert!(committed && session.committed);
        assert_eq!(
            session.io.codes,
            [0x9128, 0x9129, 0x9129, 0x9129, 0x912a, 0x912b, 0x912c]
        );
        assert_eq!(session.io.body, data);
        assert_eq!(session.io.calls.last(), Some(&(0xc3, 120000)));
        assert_eq!(session.timeout, 10000);
    }
    #[test]
    fn rejected_commit_does_not_record_success() {
        let mut session = Session::new(Fake {
            state: 2,
            reject_commit: true,
            ..Default::default()
        });
        let mut recorded = false;
        assert!(
            session
                .upload(
                    &vec![0; 130720],
                    131072,
                    |_| (),
                    || {
                        recorded = true;
                        Ok(())
                    },
                    |_| ()
                )
                .is_err()
        );
        assert!(!recorded && !session.committed && session.validated && session.idle);
        assert_eq!(session.timeout, 10000);
    }
    #[test]
    fn pending_validation_never_commits() {
        let mut session = Session::new(Fake {
            state: 1,
            ..Default::default()
        });
        assert!(
            session
                .upload(
                    &vec![0; 130720],
                    131072,
                    |_| (),
                    || panic!("must not commit"),
                    |_| ()
                )
                .is_err()
        );
        assert!(!session.io.codes.contains(&0x912c));
    }
    #[test]
    fn commit_requires_validation_and_no_parameters() {
        let mut session = Session::new(Fake::default());
        assert!(session.operation(0x912c, &[], false, None).is_err());
        session.validated = true;
        assert!(session.operation(0x912c, &[1], false, None).is_err());
        assert!(session.operation(0x912c, &[], false, Some(&[1])).is_err());
        assert!(session.io.calls.is_empty());
    }
    #[test]
    fn rejects_wrong_ptp_transaction() {
        assert!(parse_container(&container(3, 0x2001, 3, &[]), 3, None, 2).is_err());
        assert!(parse_container(&[0; 11], 3, None, 1).is_err());
    }
    #[test]
    fn real_commit_capture_has_expected_framing() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/commit-framing.json")).unwrap();
        let decode = |key: &str| {
            fixture[key]
                .as_str()
                .unwrap()
                .as_bytes()
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
                .collect::<Vec<_>>()
        };
        let cmd = decode("command");
        let response = decode("response");
        let txn = u32::from_le_bytes(cmd[8..12].try_into().unwrap());
        assert_eq!(&container(1, 0x912c, txn, &[]), &cmd[..12]);
        assert_eq!(parse_container(&response, 3, None, txn).unwrap().0, 0x2001);
    }
}
