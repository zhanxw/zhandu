//! Transactional per-user scan snapshots. Cached data is never deletion authority.
use crate::{
    model::{Node, Tree, Usage},
    platform,
    scan::{Progress, ScanOptions},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const APP_ID: i64 = 0x5A445531;
const VERSION: i64 = 1;

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub enabled: bool,
    pub path: Option<PathBuf>,
}
impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Cache {
    path: PathBuf,
    root: PathBuf,
    identity: (u64, u64),
    key: Vec<u8>,
    started: i64,
}

fn error(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn default_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Caches"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|p| Path::new(p).is_absolute())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".cache")));
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    base.filter(|p| p.is_absolute())
        .map(|p| p.join("zhandu").join("scans.sqlite3"))
}

#[cfg(unix)]
fn encode(value: &OsStr) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().to_vec()
}
#[cfg(unix)]
fn decode(value: Vec<u8>) -> io::Result<OsString> {
    use std::os::unix::ffi::OsStringExt;
    Ok(OsString::from_vec(value))
}
#[cfg(windows)]
fn encode(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}
#[cfg(windows)]
fn decode(value: Vec<u8>) -> io::Result<OsString> {
    use std::os::windows::ffi::OsStringExt;
    if !value.len().is_multiple_of(2) {
        return Err(error("Invalid cached UTF-16 filename"));
    }
    Ok(OsString::from_wide(
        &value
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_le_bytes(b))
            .collect::<Vec<_>>(),
    ))
}

fn owner_key(id: u32) -> io::Result<String> {
    #[cfg(unix)]
    {
        Ok(id.to_string())
    }
    #[cfg(windows)]
    {
        platform::owner_identifier(id).ok_or_else(|| error("Missing owner SID"))
    }
}
fn owner_id(value: String, owners: &mut HashMap<String, u32>) -> io::Result<u32> {
    if let Some(&id) = owners.get(&value) {
        return Ok(id);
    }
    #[cfg(unix)]
    let id = value.parse().map_err(error)?;
    #[cfg(windows)]
    let id = platform::resolve_owner(&value, false).map_err(error)?;
    owners.insert(value, id);
    Ok(id)
}

impl Cache {
    pub fn new(options: &ScanOptions, config: &CacheConfig) -> io::Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        let path = config
            .path
            .clone()
            .or_else(default_path)
            .ok_or_else(|| error("No user cache directory; use --cache-db PATH"))?;
        let root = fs::canonicalize(&options.path)?;
        let metadata = platform::read_metadata(&root)?;
        let identity = (metadata.dev, metadata.ino);
        let mut excludes: Vec<_> = options.excludes.iter().map(|s| encode(s)).collect();
        excludes.sort();
        excludes.dedup();
        let key = serde_json::to_vec(&(
            std::env::consts::OS,
            encode(root.as_os_str()),
            identity,
            options.one_file_system,
            excludes,
        ))
        .map_err(error)?;
        let started = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(i64::MAX as u128) as i64;
        Ok(Some(Self {
            path,
            root,
            identity,
            key,
            started,
        }))
    }

    fn open(&self) -> io::Result<Connection> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if !parent.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(parent)?;
        }
        let mut create = fs::OpenOptions::new();
        create.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            create.mode(0o600);
        }
        match create.open(&self.path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let stat = fs::symlink_metadata(&self.path)?;
        if !stat.is_file() || stat.file_type().is_symlink() {
            return Err(error("Cache must be a regular file, not a symlink"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if stat.uid() != uzers::get_effective_uid() || stat.mode() & 0o077 != 0 {
                return Err(error("Cache must be owned by this user with mode 0600"));
            }
        }
        let conn = Connection::open(&self.path).map_err(error)?;
        conn.busy_timeout(Duration::from_millis(150))
            .map_err(error)?;
        let app: i64 = conn
            .pragma_query_value(None, "application_id", |r| r.get(0))
            .map_err(error)?;
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(error)?;
        if app == 0 && version == 0 {
            let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'", [], |r| r.get(0)).map_err(error)?;
            if tables != 0 {
                return Err(error("Not a zhandu cache database"));
            }
            conn.execute_batch("BEGIN IMMEDIATE;
                CREATE TABLE IF NOT EXISTS snapshots (id INTEGER PRIMARY KEY, key BLOB NOT NULL UNIQUE, started INTEGER NOT NULL, saved INTEGER NOT NULL, elapsed REAL NOT NULL);
                CREATE TABLE IF NOT EXISTS nodes (snapshot INTEGER NOT NULL, id INTEGER NOT NULL, parent INTEGER, name BLOB NOT NULL, dev INTEGER NOT NULL, ino INTEGER NOT NULL, uid TEXT NOT NULL, gid TEXT NOT NULL, is_dir INTEGER NOT NULL, symlink INTEGER NOT NULL, shared INTEGER NOT NULL, allocated INTEGER NOT NULL, apparent INTEGER NOT NULL, inodes INTEGER NOT NULL, PRIMARY KEY(snapshot,id), FOREIGN KEY(snapshot) REFERENCES snapshots(id) ON DELETE CASCADE);
                PRAGMA application_id=0x5A445531; PRAGMA user_version=1; COMMIT;").map_err(error)?;
        } else if app != APP_ID || version != VERSION {
            return Err(error("Unsupported cache database format"));
        }
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(error)?;
        Ok(conn)
    }

    pub fn load(&self, progress: &Progress) -> io::Result<Option<(Tree, i64)>> {
        // Do not create a database until there is a completed scan to save.
        if !self.path.exists() {
            return Ok(None);
        }
        let mut conn = self.open()?;
        let tx = conn.transaction().map_err(error)?;
        let info = tx
            .query_row(
                "SELECT id,saved,elapsed FROM snapshots WHERE key=?1",
                [&self.key],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, f64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(error)?;
        let Some((snapshot, saved, elapsed)) = info else {
            return Ok(None);
        };
        let mut tree = Tree {
            root: self.root.clone(),
            nodes: vec![],
            errors: vec![],
            error_count: 0,
            elapsed_seconds: elapsed,
        };
        let mut owners = HashMap::new();
        let mut statement = tx.prepare("SELECT id,parent,name,dev,ino,uid,gid,is_dir,symlink,shared,allocated,apparent,inodes FROM nodes WHERE snapshot=?1 ORDER BY id").map_err(error)?;
        let mut rows = statement.query([snapshot]).map_err(error)?;
        while let Some(row) = rows.next().map_err(error)? {
            if progress.is_cancelled() {
                return Err(error("Cache load cancelled"));
            }
            let id = usize::try_from(row.get::<_, i64>(0).map_err(error)?).map_err(error)?;
            let parent = row
                .get::<_, Option<i64>>(1)
                .map_err(error)?
                .map(usize::try_from)
                .transpose()
                .map_err(error)?;
            let name = decode(row.get(2).map_err(error)?)?;
            if id != tree.nodes.len()
                || (id == 0 && parent.is_some())
                || (id > 0 && parent.is_none_or(|p| p >= id || !tree.nodes[p].is_dir))
            {
                return Err(error("Invalid cached tree structure"));
            }
            if id > 0 {
                let mut components = Path::new(&name).components();
                if !matches!(components.next(), Some(Component::Normal(_)))
                    || components.next().is_some()
                {
                    return Err(error("Invalid cached filename"));
                }
            }
            let own = Usage {
                allocated: row.get::<_, i64>(10).map_err(error)? as u64,
                apparent: row.get::<_, i64>(11).map_err(error)? as u64,
                inodes: row.get::<_, i64>(12).map_err(error)? as u64,
            };
            let node = Node {
                identity: (
                    row.get::<_, i64>(3).map_err(error)? as u64,
                    row.get::<_, i64>(4).map_err(error)? as u64,
                ),
                name,
                parent,
                children: vec![],
                uid: owner_id(row.get(5).map_err(error)?, &mut owners)?,
                gid: owner_id(row.get(6).map_err(error)?, &mut owners)?,
                is_dir: row.get(7).map_err(error)?,
                is_symlink: row.get(8).map_err(error)?,
                shared_link: row.get(9).map_err(error)?,
                own,
                total: own,
                owners: HashMap::new(),
            };
            tree.nodes.push(node);
            if let Some(parent) = parent {
                tree.nodes[parent].children.push(id);
            }
        }
        if tree
            .nodes
            .first()
            .is_none_or(|n| !n.is_dir || n.identity != self.identity)
        {
            return Err(error("Cached root no longer matches"));
        }
        tree.aggregate();
        Ok(Some((tree, now().saturating_sub(saved).max(0))))
    }

    /// Replace one snapshot atomically; cancelled/incomplete scans never overwrite it.
    pub fn save(&self, tree: &Tree, progress: &Progress) -> io::Result<()> {
        if tree.error_count != 0 || progress.is_cancelled() {
            return Err(error("Incomplete refresh; previous cache retained"));
        }
        let mut conn = self.open()?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        let previous: Option<i64> = tx
            .query_row(
                "SELECT started FROM snapshots WHERE key=?1",
                [&self.key],
                |r| r.get(0),
            )
            .optional()
            .map_err(error)?;
        if previous.is_some_and(|started| started > self.started) {
            return Ok(());
        }
        tx.execute("DELETE FROM snapshots WHERE key=?1", [&self.key])
            .map_err(error)?;
        tx.execute(
            "INSERT INTO snapshots (key,started,saved,elapsed) VALUES (?1,?2,?3,?4)",
            params![self.key, self.started, now(), tree.elapsed_seconds],
        )
        .map_err(error)?;
        let snapshot = tx.last_insert_rowid();
        {
            let mut statement = tx
                .prepare(
                    "INSERT INTO nodes VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                )
                .map_err(error)?;
            let mut owners = HashMap::new();
            for (id, node) in tree.nodes.iter().enumerate() {
                if progress.is_cancelled() {
                    return Err(error("Cache save cancelled; previous cache retained"));
                }
                for owner in [node.uid, node.gid] {
                    if let std::collections::hash_map::Entry::Vacant(entry) = owners.entry(owner) {
                        entry.insert(owner_key(owner)?);
                    }
                }
                statement
                    .execute(params![
                        snapshot,
                        id as i64,
                        node.parent.map(|p| p as i64),
                        encode(&node.name),
                        node.identity.0 as i64,
                        node.identity.1 as i64,
                        owners[&node.uid],
                        owners[&node.gid],
                        node.is_dir,
                        node.is_symlink,
                        node.shared_link,
                        node.own.allocated as i64,
                        node.own.apparent as i64,
                        node.own.inodes as i64
                    ])
                    .map_err(error)?;
            }
        }
        // Retain the 16 most recently saved roots/configurations; freed pages are reused.
        tx.execute("DELETE FROM snapshots WHERE key NOT IN (SELECT key FROM snapshots ORDER BY saved DESC,started DESC LIMIT 16)", []).map_err(error)?;
        if progress.is_cancelled() {
            return Err(error("Cache save cancelled; previous cache retained"));
        }
        tx.commit().map_err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::scan;
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, ScanOptions, CacheConfig) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        fs::write(root.join("nested/data"), [7; 8192]).unwrap();
        fs::hard_link(root.join("nested/data"), root.join("link")).unwrap();
        let options = ScanOptions {
            path: root,
            threads: 2,
            one_file_system: false,
            excludes: vec![],
        };
        let config = CacheConfig {
            enabled: true,
            path: Some(dir.path().join("cache/scans.sqlite3")),
        };
        (dir, options, config)
    }

    #[test]
    fn snapshot_roundtrip_preserves_paths_owners_hardlinks_and_totals() {
        let (_dir, options, config) = fixture();
        let cache = Cache::new(&options, &config).unwrap().unwrap();
        let tree = scan(options.clone(), Arc::default()).unwrap();
        assert!(cache.load(&Progress::default()).unwrap().is_none());
        assert!(!cache.path.exists());
        cache.save(&tree, &Progress::default()).unwrap();
        let (loaded, age) = cache.load(&Progress::default()).unwrap().unwrap();
        assert!(age >= 0);
        assert_eq!(loaded.nodes.len(), tree.nodes.len());
        for (id, node) in tree.nodes.iter().enumerate() {
            let other = &loaded.nodes[id];
            assert_eq!(tree.path(id), loaded.path(id));
            assert_eq!(
                (
                    node.uid,
                    node.gid,
                    node.identity,
                    node.shared_link,
                    node.own,
                    node.total
                ),
                (
                    other.uid,
                    other.gid,
                    other.identity,
                    other.shared_link,
                    other.own,
                    other.total
                )
            );
            assert_eq!(node.owners, other.owners);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&cache.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let mut different = options.clone();
        different.threads = 1;
        assert!(
            Cache::new(&different, &config)
                .unwrap()
                .unwrap()
                .load(&Progress::default())
                .unwrap()
                .is_some()
        );
        different.excludes.push("nested".into());
        assert!(
            Cache::new(&different, &config)
                .unwrap()
                .unwrap()
                .load(&Progress::default())
                .unwrap()
                .is_none()
        );
        different.excludes.clear();
        different.one_file_system = true;
        assert!(
            Cache::new(&different, &config)
                .unwrap()
                .unwrap()
                .load(&Progress::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn fresh_snapshot_removes_missing_entries_and_incomplete_refresh_keeps_previous() {
        let (_dir, options, config) = fixture();
        let cache = Cache::new(&options, &config).unwrap().unwrap();
        let tree = scan(options.clone(), Arc::default()).unwrap();
        cache.save(&tree, &Progress::default()).unwrap();
        fs::remove_file(options.path.join("link")).unwrap();
        fs::write(options.path.join("new"), b"new data").unwrap();
        let mut fresh = scan(options, Arc::default()).unwrap();
        let cancelled = Progress::default();
        cancelled.cancel();
        assert!(cache.save(&fresh, &cancelled).is_err());
        fresh.error_count = 1;
        assert!(cache.save(&fresh, &Progress::default()).is_err());
        let old = cache.load(&Progress::default()).unwrap().unwrap().0;
        assert!(old.nodes.iter().any(|n| n.name == "link"));
        fresh.error_count = 0;
        cache.save(&fresh, &Progress::default()).unwrap();
        let loaded = cache.load(&Progress::default()).unwrap().unwrap().0;
        assert!(!loaded.nodes.iter().any(|n| n.name == "link"));
        assert!(loaded.nodes.iter().any(|n| n.name == "new"));
        assert_eq!(loaded.nodes[0].total, fresh.nodes[0].total);
    }

    #[test]
    fn rollback_and_older_scans_cannot_replace_newer_snapshot() {
        let (_dir, options, config) = fixture();
        let older = Cache::new(&options, &config).unwrap().unwrap();
        let old = scan(options.clone(), Arc::default()).unwrap();
        let mut newer = older.clone();
        newer.started += 1;
        fs::write(options.path.join("new"), b"new").unwrap();
        let fresh = scan(options, Arc::default()).unwrap();
        newer.save(&fresh, &Progress::default()).unwrap();
        older.save(&old, &Progress::default()).unwrap();
        {
            let mut conn = newer.open().unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute("DELETE FROM snapshots", []).unwrap();
            // Simulate an interrupted update: Drop rolls back the transaction.
        }
        assert_eq!(
            newer.load(&Progress::default()).unwrap().unwrap().0.nodes[0].total,
            fresh.nodes[0].total
        );
    }

    #[test]
    fn disabled_corrupt_and_invalid_cache_do_not_provide_results() {
        let (_dir, options, mut config) = fixture();
        config.enabled = false;
        assert!(Cache::new(&options, &config).unwrap().is_none());
        assert!(!config.path.as_ref().unwrap().exists());
        config.enabled = true;
        let cache = Cache::new(&options, &config).unwrap().unwrap();
        let tree = scan(options, Arc::default()).unwrap();
        cache.save(&tree, &Progress::default()).unwrap();
        let conn = cache.open().unwrap();
        conn.execute(
            "UPDATE nodes SET name=?1 WHERE id=1",
            [encode(OsStr::new("../escape"))],
        )
        .unwrap();
        assert!(cache.load(&Progress::default()).is_err());
        drop(conn);
        fs::write(&cache.path, b"not SQLite").unwrap();
        assert!(cache.load(&Progress::default()).is_err());
    }

    #[test]
    fn filename_encoding_is_lossless() {
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![b'a', 0xff, b'z'])
        };
        #[cfg(windows)]
        let name = {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(&[97, 0xd800, 122])
        };
        assert_eq!(decode(encode(&name)).unwrap(), name);
    }
}
