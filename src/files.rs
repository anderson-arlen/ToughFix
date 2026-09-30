// SPDX-License-Identifier: MIT
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
};

pub fn atomic(path: &Path, content: &[u8]) -> Result<()> {
    atomic_mode(path, content, None)
}
pub fn atomic_mode(path: &Path, content: &[u8], mode: Option<u32>) -> Result<()> {
    let parent = path.parent().context("Missing destination directory")?;
    fs::create_dir_all(parent)?;
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    if let Some(mode) = mode {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    file.write_all(content)?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub fn json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    atomic(path, &serde_json::to_vec_pretty(value)?)
}
