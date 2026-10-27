//! Explicit file-only deletion; never recursively remove a directory.
use crate::{
    model::Tree,
    platform::{EntryMetadata, read_metadata},
    scan::Progress,
};
use std::{collections::HashMap, fs, io, path::PathBuf, time::SystemTime};

#[derive(Clone, Debug)]
pub(crate) struct Mark {
    pub path: PathBuf,
    pub metadata: EntryMetadata,
    modified: SystemTime,
    // Includes the scan root. Each directory must still be the scanned object.
    ancestors: Vec<(PathBuf, (u64, u64))>,
}

impl Mark {
    pub fn new(tree: &Tree, id: usize) -> io::Result<Self> {
        let node = &tree.nodes[id];
        let path = tree.path(id);
        let metadata = read_metadata(&path)?;
        let stat = fs::symlink_metadata(&path)?;
        if node.is_dir || node.is_symlink || !stat.is_file() || metadata.is_symlink {
            return Err(io::Error::other(
                "Only regular files can be marked; open directories with Enter",
            ));
        }
        if (metadata.dev, metadata.ino) != node.identity
            || metadata.uid != node.uid
            || metadata.gid != node.gid
        {
            return Err(io::Error::other(
                "File changed since scanning; restart the scan",
            ));
        }
        let mut ancestors = Vec::new();
        let mut parent = node.parent;
        while let Some(id) = parent {
            ancestors.push((tree.path(id), tree.nodes[id].identity));
            parent = tree.nodes[id].parent;
        }
        ancestors.reverse();
        Ok(Self {
            path,
            metadata,
            modified: stat.modified()?,
            ancestors,
        })
    }

    fn verify(
        &self,
        metadata: &EntryMetadata,
        modified: SystemTime,
        regular: bool,
    ) -> io::Result<()> {
        let old = &self.metadata;
        if !regular
            || metadata.is_symlink
            || metadata.is_dir
            || (
                metadata.dev,
                metadata.ino,
                metadata.uid,
                metadata.gid,
                metadata.usage.apparent,
            ) != (old.dev, old.ino, old.uid, old.gid, old.usage.apparent)
            || modified != self.modified
        {
            return Err(io::Error::other("File changed since marking; skipped"));
        }
        Ok(())
    }
}

pub(crate) fn estimate<'a>(marks: impl Iterator<Item = &'a Mark>) -> u64 {
    let mut groups: HashMap<(u64, u64), (u64, u64, u64)> = HashMap::new();
    for mark in marks {
        let m = &mark.metadata;
        let entry = groups
            .entry((m.dev, m.ino))
            .or_insert((0, m.nlink, m.usage.allocated));
        entry.0 += 1;
        entry.1 = entry.1.max(m.nlink);
    }
    groups
        .values()
        .filter(|(count, links, _)| count >= links)
        .map(|(_, _, bytes)| bytes)
        .sum()
}

#[derive(Default, Debug)]
pub(crate) struct Outcome {
    pub deleted: usize,
    pub reclaimed: u64,
    pub failures: Vec<(Mark, String)>,
}

pub(crate) fn execute(marks: Vec<Mark>, progress: &Progress) -> Outcome {
    let mut result = Outcome::default();
    for mark in marks {
        let deletion = if progress.is_cancelled() {
            Err(io::Error::other("Cancelled before deletion"))
        } else {
            remove(&mark)
        };
        match deletion {
            Ok(bytes) => {
                result.deleted += 1;
                result.reclaimed = result.reclaimed.saturating_add(bytes);
            }
            Err(error) => result.failures.push((mark, error.to_string())),
        }
    }
    result
}

#[cfg(unix)]
fn remove(mark: &Mark) -> io::Result<u64> {
    use rustix::fs::{AtFlags, Mode, OFlags, fstat, open, openat, statat, unlinkat};
    use std::os::unix::fs::MetadataExt;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut parent = None;
    for (path, identity) in &mark.ancestors {
        let fd = match &parent {
            None => open(path, flags, Mode::empty())?,
            Some(fd) => openat(fd, path.file_name().unwrap(), flags, Mode::empty())?,
        };
        let stat = fstat(&fd)?;
        if (stat.st_dev as u64, stat.st_ino as u64) != *identity {
            return Err(io::Error::other(
                "Parent directory changed since scanning; skipped",
            ));
        }
        parent = Some(fd);
    }
    let parent = parent.ok_or_else(|| io::Error::other("Cannot delete scan root"))?;
    let name = mark.path.file_name().unwrap();
    let fd = openat(
        &parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let file = fs::File::from(fd);
    let stat = file.metadata()?;
    let metadata = EntryMetadata {
        dev: stat.dev(),
        ino: stat.ino(),
        nlink: stat.nlink(),
        uid: stat.uid(),
        gid: stat.gid(),
        is_dir: stat.is_dir(),
        is_symlink: false,
        usage: crate::model::Usage {
            allocated: stat.blocks().saturating_mul(512),
            apparent: stat.len(),
            inodes: 1,
        },
    };
    mark.verify(&metadata, stat.modified()?, stat.is_file())?;
    // Anchored directory handles prevent an ancestor symlink swap escaping the tree.
    // POSIX has no atomic compare-inode-and-unlink: a final-name rename race remains.
    let current = statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if (current.st_dev as u64, current.st_ino as u64) != (metadata.dev, metadata.ino) {
        return Err(io::Error::other("File replaced before deletion; skipped"));
    }
    unlinkat(&parent, name, AtFlags::empty())?;
    Ok(if metadata.nlink == 1 {
        metadata.usage.allocated
    } else {
        0
    })
}

#[cfg(windows)]
fn remove(mark: &Mark) -> io::Result<u64> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, READ_CONTROL,
        SetFileInformationByHandle,
    };
    let mut guards = Vec::new();
    for (path, identity) in &mark.ancestors {
        // Deny rename/deletion of each ancestor until the file has been removed.
        let file = fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let m = crate::platform::metadata_from_file(&file)?;
        if !m.is_dir || m.is_symlink || (m.dev, m.ino) != *identity {
            return Err(io::Error::other(
                "Parent directory changed since scanning; skipped",
            ));
        }
        guards.push(file);
    }
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL | DELETE)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(&mark.path)?;
    let metadata = crate::platform::metadata_from_file(&file)?;
    let stat = file.metadata()?;
    mark.verify(&metadata, stat.modified()?, stat.is_file())?;
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: live owned file handle, correctly sized disposition buffer. Deletes this
    // verified handle's file on close, without another pathname lookup.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    drop(file);
    Ok(if metadata.nlink == 1 {
        metadata.usage.allocated
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::sync::Arc;
    fn tree(path: &std::path::Path) -> Tree {
        scan(
            ScanOptions {
                path: path.into(),
                threads: 2,
                one_file_system: false,
                excludes: vec![],
            },
            Arc::default(),
        )
        .unwrap()
    }
    fn mark(tree: &Tree, name: &str) -> Mark {
        Mark::new(
            tree,
            tree.nodes.iter().position(|n| n.name == name).unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn deletion_checks_changes_reports_partial_success_and_preserves_unmarked_files() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["delete", "changed", "keep"] {
            fs::write(dir.path().join(name), [1; 8192]).unwrap();
        }
        let tree = tree(dir.path());
        let delete = mark(&tree, "delete");
        let changed = mark(&tree, "changed");
        fs::write(&changed.path, b"changed after marking").unwrap();
        let result = execute(vec![delete.clone(), changed], &Progress::default());
        assert_eq!(result.deleted, 1);
        assert_eq!(result.reclaimed, delete.metadata.usage.allocated);
        assert_eq!(result.failures.len(), 1);
        assert!(!dir.path().join("delete").exists());
        assert!(dir.path().join("changed").exists());
        assert!(dir.path().join("keep").exists());
    }
    #[test]
    fn hardlinks_only_reclaim_when_the_last_link_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), [9; 8192]).unwrap();
        fs::hard_link(dir.path().join("a"), dir.path().join("b")).unwrap();
        let tree = tree(dir.path());
        let a = mark(&tree, "a");
        let b = mark(&tree, "b");
        assert_eq!(estimate([&a].into_iter()), 0);
        assert_eq!(estimate([&a, &b].into_iter()), a.metadata.usage.allocated);
        let first = execute(vec![a.clone()], &Progress::default());
        assert_eq!((first.deleted, first.reclaimed), (1, 0));
        let last = execute(vec![b], &Progress::default());
        assert_eq!(
            (last.deleted, last.reclaimed),
            (1, a.metadata.usage.allocated)
        );
    }
    #[test]
    fn cancellation_and_replaced_files_are_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), b"keep").unwrap();
        let tree = tree(dir.path());
        assert!(Mark::new(&tree, 0).is_err());
        let file = mark(&tree, "file");
        let progress = Progress::default();
        progress.cancel();
        assert_eq!(execute(vec![file.clone()], &progress).deleted, 0);
        fs::rename(&file.path, dir.path().join("old")).unwrap();
        fs::write(&file.path, b"keep").unwrap();
        assert_eq!(execute(vec![file], &Progress::default()).deleted, 0);
        assert!(dir.path().join("file").exists());
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_and_swapped_ancestors_never_delete_external_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/file"), b"local").unwrap();
        fs::write(outside.path().join("file"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path().join("file"), dir.path().join("link")).unwrap();
        let tree = tree(dir.path());
        assert!(
            Mark::new(
                &tree,
                tree.nodes.iter().position(|n| n.name == "link").unwrap()
            )
            .is_err()
        );
        let marked = mark(&tree, "file");
        fs::rename(dir.path().join("nested"), dir.path().join("old")).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("nested")).unwrap();
        assert_eq!(execute(vec![marked], &Progress::default()).deleted, 0);
        assert_eq!(fs::read(outside.path().join("file")).unwrap(), b"outside");
        assert!(dir.path().join("old/file").exists());
    }
}
