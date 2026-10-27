//! Platform-specific metadata and account identities.
use crate::model::{Usage, display_name};
use std::{io, path::Path};

#[derive(Debug)]
pub(crate) struct EntryMetadata {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub usage: Usage,
}

#[cfg(unix)]
pub(crate) fn read_metadata(path: &Path) -> io::Result<EntryMetadata> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    Ok(EntryMetadata {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        is_dir: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        usage: Usage {
            allocated: metadata.blocks().saturating_mul(512),
            apparent: metadata.size(),
            inodes: 1,
        },
    })
}

#[cfg(unix)]
pub fn resolve_owner(value: &str, group: bool) -> Result<u32, String> {
    if let Ok(id) = value.parse() {
        return Ok(id);
    }
    if group {
        uzers::get_group_by_name(value).map(|g| g.gid())
    } else {
        uzers::get_user_by_name(value).map(|u| u.uid())
    }
    .ok_or_else(|| format!("unknown {}: {value}", if group { "group" } else { "user" }))
}

#[cfg(unix)]
pub fn owner_name(id: u32, group: bool) -> Option<String> {
    if group {
        uzers::get_group_by_gid(id).map(|g| display_name(g.name()))
    } else {
        uzers::get_user_by_uid(id).map(|u| display_name(u.name()))
    }
}

#[cfg(unix)]
pub fn owner_identifier(_: u32) -> Option<String> {
    None
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::read_metadata;
#[cfg(windows)]
pub use windows::{owner_identifier, owner_name, resolve_owner};
