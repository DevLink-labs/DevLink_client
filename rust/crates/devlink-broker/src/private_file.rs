//! Bounded private snapshots. Credentials never appear in errors.
use crate::credentials;
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

pub fn read<T: DeserializeOwned>(path: &Path, limit: usize) -> Result<Option<T>> {
    read_bytes(path, limit)?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid private snapshot"))
        })
        .transpose()
}

pub fn read_bytes(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => anyhow::bail!("Cannot inspect private snapshot"),
        Ok(_) => {}
    }
    verify_snapshot_path(path, limit)?;
    credentials::verify_private_permissions(path)?;
    read_bounded(path, limit).map(Some)
}

/// Bound both the opened file's metadata and the actual read. Callers apply their own
/// path and permission policy before calling this.
pub(crate) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = fs::File::open(path).context("Cannot open private snapshot")?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Private snapshot is not a regular file");
    ensure!(
        metadata.len() <= limit as u64,
        "Private snapshot exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("Cannot read private snapshot")?;
    ensure!(bytes.len() <= limit, "Private snapshot exceeds size limit");
    Ok(bytes)
}

pub fn write<T: Serialize>(path: &Path, value: &T, limit: usize) -> Result<()> {
    let parent = path.parent().context("Private snapshot has no parent")?;
    credentials::ensure_safe_parent_directory(parent)?;
    if fs::symlink_metadata(path).is_ok() {
        verify_snapshot_path(path, limit)?;
        credentials::verify_private_permissions(path)?;
    }
    credentials::set_private_permissions(parent)?;
    let bytes =
        serde_json::to_vec(value).map_err(|_| anyhow::anyhow!("Cannot encode private snapshot"))?;
    ensure!(bytes.len() <= limit, "Private snapshot exceeds size limit");
    let mut temp = tempfile::Builder::new()
        .prefix(".private-")
        .tempfile_in(parent)
        .context("Cannot prepare private snapshot")?;
    credentials::set_private_permissions(temp.path())?;
    temp.write_all(&bytes)
        .context("Cannot write private snapshot")?;
    temp.as_file()
        .sync_all()
        .context("Cannot flush private snapshot")?;
    credentials::verify_private_permissions(temp.path())?;
    temp.persist(path)
        .map_err(|_| anyhow::anyhow!("Cannot replace private snapshot"))?;
    verify_snapshot_path(path, limit)?;
    credentials::verify_private_permissions(path)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

// Private data snapshots use their caller's bound. MCP credential files keep
// credentials::verify_path_safety's independent 16KiB limit.
fn verify_snapshot_path(path: &Path, limit: usize) -> Result<()> {
    ensure!(
        credentials::ensure_safe_target_if_exists(path)?,
        "Private snapshot disappeared"
    );
    credentials::verify_parent_safety(path.parent().context("Private snapshot has no parent")?)?;
    ensure!(
        fs::metadata(path)?.len() <= limit as u64,
        "Private snapshot exceeds size limit"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn data_snapshot_bound_does_not_change_the_credentials_bound() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.json");
        let value = "x".repeat(20 * 1024);
        write(&path, &value, 32 * 1024).unwrap();
        assert_eq!(
            read::<String>(&path, 32 * 1024).unwrap().as_deref(),
            Some(value.as_str())
        );
        assert!(read::<String>(&path, 16 * 1024).is_err());
        assert!(write(&path, &value, 16 * 1024).is_err());
        assert!(credentials::verify_path_safety(&path).is_err());
        credentials::verify_private_permissions(&path).unwrap();
    }
}
