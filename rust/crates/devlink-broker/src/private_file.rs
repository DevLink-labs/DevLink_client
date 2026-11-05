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
    persist_private(temp, path, limit)?;
    verify_snapshot_path(path, limit)?;
    credentials::verify_private_permissions(path)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(all(test, windows))]
std::thread_local! {
    static PERSIST_RETRY_SIGNAL: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

fn persist_private(mut temp: tempfile::NamedTempFile, path: &Path, limit: usize) -> Result<()> {
    for attempt in 0..10 {
        match temp.persist(path) {
            Ok(_) => return Ok(()),
            Err(error) => {
                // Match the inbox's bounded Windows replacement retry policy.
                let transient = cfg!(windows)
                    && (error.error.kind() == std::io::ErrorKind::PermissionDenied
                        || matches!(error.error.raw_os_error(), Some(32 | 33)));
                if !transient || attempt == 9 {
                    return Err(error.error).context("Cannot replace private snapshot");
                }
                temp = error.file;
                #[cfg(all(test, windows))]
                PERSIST_RETRY_SIGNAL.with(|signal| {
                    if let Some(sender) = signal.borrow_mut().take() {
                        let _ = sender.send(());
                    }
                });
                std::thread::sleep(std::time::Duration::from_millis(50));
                // A delayed replacement must not bypass the original path/ACL checks.
                credentials::verify_parent_safety(
                    path.parent().context("Private snapshot has no parent")?,
                )?;
                credentials::verify_private_permissions(temp.path())?;
                if credentials::ensure_safe_target_if_exists(path)? {
                    verify_snapshot_path(path, limit)?;
                    credentials::verify_private_permissions(path)?;
                }
            }
        }
    }
    unreachable!("bounded persistence loop always returns")
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

    #[cfg(windows)]
    #[test]
    fn temporary_reader_does_not_abort_private_snapshot_replacement() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-profile.json");
        write(&path, &"before", 1024).unwrap();
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        let (signal, waiting) = std::sync::mpsc::channel();
        PERSIST_RETRY_SIGNAL.with(|slot| *slot.borrow_mut() = Some(signal));
        let holder = std::thread::spawn(move || {
            waiting
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            drop(reader);
        });
        let result = write(&path, &"after", 1024);
        PERSIST_RETRY_SIGNAL.with(|slot| slot.borrow_mut().take());
        holder.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            read::<String>(&path, 1024).unwrap().as_deref(),
            Some("after")
        );
        credentials::verify_private_permissions(&path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn persistent_reader_preserves_the_original_snapshot_and_returns_an_io_cause() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-profile.json");
        write(&path, &"before", 1024).unwrap();
        let original = fs::read(&path).unwrap();
        let reader = fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&path)
            .unwrap();
        let started = std::time::Instant::now();
        let error = write(&path, &"after", 1024).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        drop(reader);
        write(&path, &"after", 1024).unwrap();
        assert_eq!(
            read::<String>(&path, 1024).unwrap().as_deref(),
            Some("after")
        );
    }

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
