use super::{ConfigError, Result};
use crate::credentials;
use std::{fs, path::Path};
fn unsafe_file() -> ConfigError {
    ConfigError::new("SETUP_UNSAFE_FILE")
}
pub(super) fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let builder = fs::DirBuilder::new();
    #[cfg(unix)]
    let mut builder = builder;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
/// A stable alias is rendered to the Host, so its parent must also resist replacement.
pub(super) fn source_alias(path: &Path) -> Result<()> {
    let info = fs::symlink_metadata(path).map_err(|_| unsafe_file().at(path))?;
    if !info.is_file() && !info.is_symlink() {
        return Err(unsafe_file().at(path));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = unsafe { libc::getuid() };
        if info.uid() != uid && info.uid() != 0 {
            return Err(unsafe_file().at(path));
        }
    }
    let parent = path.parent().ok_or_else(|| unsafe_file().at(path))?;
    credentials::verify_state_directory_permissions(parent)
        .map_err(|e| ConfigError::unsafe_at(parent, e))
}
#[cfg(unix)]
pub(super) fn copy_host_permissions(source: &Path, target: &Path) -> Result<()> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let metadata = fs::metadata(source).map_err(|_| unsafe_file())?;
    let file = fs::File::open(target).map_err(|_| unsafe_file())?;
    // Preserve group read access too; fail before replacement if this group cannot be retained.
    if unsafe { libc::fchown(file.as_raw_fd(), libc::uid_t::MAX, metadata.gid()) } != 0 {
        return Err(unsafe_file());
    }
    fs::set_permissions(target, metadata.permissions()).map_err(|_| unsafe_file())
}
#[cfg(windows)]
pub(super) fn copy_host_permissions(source: &Path, target: &Path) -> Result<()> {
    crate::win_acl::copy_dacl(source, target).map_err(|_| unsafe_file())
}
