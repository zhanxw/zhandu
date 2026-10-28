use crate::model::{Node, Tree, Usage, display_name};
use crate::platform::{EntryMetadata, read_metadata};
use rayon::{Scope, prelude::*};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs, io,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    time::Instant,
};

const METADATA_BATCH: usize = 64;
const PARALLEL_METADATA_THRESHOLD: usize = 256;
const MAX_ERRORS: usize = 100;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub path: PathBuf,
    pub threads: usize,
    pub one_file_system: bool,
    pub excludes: Vec<OsString>,
}

#[derive(Default)]
pub struct Progress {
    pub entries: AtomicU64,
    pub errors: AtomicU64,
    /// 0: scanning, 1: building the tree and aggregating, 2: complete
    pub phase: AtomicU8,
    pub cancelled: AtomicBool,
}

impl Progress {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

struct Entry {
    name: OsString,
    metadata: EntryMetadata,
    directory: Option<usize>,
}

struct DirectoryBatch {
    id: usize,
    entries: Vec<Entry>,
    errors: Vec<String>,
    error_count: u64,
}

impl DirectoryBatch {
    fn error(&mut self, message: String) {
        self.error_count += 1;
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(display_name(message.as_ref()));
        }
    }
}

struct Context<'a> {
    options: &'a ScanOptions,
    progress: &'a Progress,
    root_device: u64,
    next_directory: AtomicUsize,
    live: Option<&'a SyncSender<LiveEvent>>,
}

/// Enumerate one directory, then distribute its metadata reads across the same
/// pool that schedules other directories. No worker waits on a directory queue.
fn read_directory(path: &PathBuf, id: usize, context: &Context<'_>) -> DirectoryBatch {
    let mut batch = DirectoryBatch {
        id,
        entries: Vec::new(),
        errors: Vec::new(),
        error_count: 0,
    };
    if context.progress.is_cancelled() {
        return batch;
    }
    let mut names = Vec::new();
    match fs::read_dir(path) {
        Err(error) => batch.error(format!("{}: {error}", display_name(path.as_os_str()))),
        Ok(entries) => {
            for entry in entries {
                if context.progress.is_cancelled() {
                    break;
                }
                match entry {
                    Err(error) => {
                        batch.error(format!("{}: {error}", display_name(path.as_os_str())))
                    }
                    Ok(entry) => {
                        // Read the name only. Metadata below supplies the file type too.
                        let name = entry.file_name();
                        if !context.options.excludes.contains(&name) {
                            names.push(name);
                        }
                    }
                }
            }
        }
    }
    let parallel = context.options.threads > 1 && names.len() >= PARALLEL_METADATA_THRESHOLD;
    if parallel {
        names.par_sort_unstable();
    } else {
        names.sort_unstable();
    }
    let mut entries: Vec<_> = names.into_iter().map(|name| (name, None)).collect();
    let inspect = |chunk: &mut [(OsString, Option<io::Result<EntryMetadata>>)]| {
        let mut inspected = 0;
        for (name, result) in chunk.iter_mut() {
            if context.progress.is_cancelled() {
                break;
            }
            *result = Some(read_metadata(&path.join(name)));
            inspected += 1;
        }
        context
            .progress
            .entries
            .fetch_add(inspected, Ordering::Relaxed);
        if let Some(sender) = context.live {
            let entries = chunk
                .iter()
                .filter_map(|(name, result)| {
                    let Some(Ok(metadata)) = result else {
                        return None;
                    };
                    if context.options.one_file_system && metadata.dev != context.root_device {
                        return None;
                    }
                    Some((name.clone(), metadata.clone()))
                })
                .collect();
            if sender
                .send(LiveEvent::Entries {
                    parent: path.clone(),
                    entries,
                })
                .is_err()
            {
                context.progress.cancel();
            }
        }
    };
    if parallel {
        entries.par_chunks_mut(METADATA_BATCH).for_each(inspect);
    } else {
        for chunk in entries.chunks_mut(METADATA_BATCH) {
            inspect(chunk);
        }
    }
    for (name, metadata) in entries {
        let metadata = match metadata {
            Some(Ok(metadata)) => metadata,
            Some(Err(error)) => {
                batch.error(format!(
                    "{}: {error}",
                    display_name(path.join(&name).as_os_str())
                ));
                continue;
            }
            None => continue, // cancelled before this entry's metadata was read
        };
        if context.options.one_file_system && metadata.dev != context.root_device {
            continue;
        }
        let directory = metadata
            .is_dir
            .then(|| context.next_directory.fetch_add(1, Ordering::Relaxed));
        batch.entries.push(Entry {
            name,
            metadata,
            directory,
        });
    }
    context
        .progress
        .errors
        .fetch_add(batch.error_count, Ordering::Relaxed);
    if batch.error_count > 0
        && let Some(sender) = context.live
    {
        let _ = sender.send(LiveEvent::Errors(batch.error_count, batch.errors.clone()));
    }

    batch
}

fn walk_parallel<'scope>(
    path: PathBuf,
    id: usize,
    context: &'scope Context<'scope>,
    sender: &'scope SyncSender<DirectoryBatch>,
    scope: &Scope<'scope>,
) {
    if context.progress.is_cancelled() {
        return;
    }
    let batch = read_directory(&path, id, context);
    let children: Vec<_> = batch
        .entries
        .iter()
        .filter_map(|entry| entry.directory.map(|id| (path.join(&entry.name), id)))
        .collect();
    if sender.send(batch).is_err() {
        return;
    }
    for (path, id) in children {
        scope.spawn(move |scope| walk_parallel(path, id, context, sender, scope));
    }
}

fn collect_batch(batch: DirectoryBatch, records: &mut Vec<Option<Vec<Entry>>>, tree: &mut Tree) {
    tree.error_count += batch.error_count;
    tree.errors.extend(
        batch
            .errors
            .into_iter()
            .take(MAX_ERRORS.saturating_sub(tree.errors.len())),
    );
    if records.len() <= batch.id {
        records.resize_with(batch.id + 1, || None);
    }
    records[batch.id] = Some(batch.entries);
}

fn append_node(
    tree: &mut Tree,
    name: OsString,
    metadata: EntryMetadata,
    parent: Option<usize>,
    hard_links: &mut HashSet<(u64, u64)>,
) -> usize {
    let shared_link =
        !metadata.is_dir && metadata.nlink > 1 && !hard_links.insert((metadata.dev, metadata.ino));
    let own = if shared_link {
        Usage::default()
    } else {
        metadata.usage
    };
    let id = tree.nodes.len();
    tree.nodes.push(Node {
        identity: (metadata.dev, metadata.ino),
        name,
        parent,
        children: Vec::new(),
        uid: metadata.uid,
        gid: metadata.gid,
        is_dir: metadata.is_dir,
        is_symlink: metadata.is_symlink,
        shared_link,
        own,
        total: own,
        owners: HashMap::new(),
    });
    if let Some(parent) = parent {
        tree.nodes[parent].children.push(id);
    }
    id
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "scan cancelled")
}

pub fn scan(options: ScanOptions, progress: Arc<Progress>) -> io::Result<Tree> {
    scan_inner(options, progress, None)
}

pub(crate) fn scan_live(
    options: ScanOptions,
    progress: Arc<Progress>,
    sender: SyncSender<LiveEvent>,
) -> io::Result<Tree> {
    scan_inner(options, progress, Some(&sender))
}

fn scan_inner(
    options: ScanOptions,
    progress: Arc<Progress>,
    live: Option<&SyncSender<LiveEvent>>,
) -> io::Result<Tree> {
    let start = Instant::now();
    let root = fs::canonicalize(&options.path)?;
    let root_metadata = read_metadata(&root)?;
    if !root_metadata.is_dir {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "scan root must be a directory",
        ));
    }
    if options.threads == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "threads must be positive",
        ));
    }
    if progress.is_cancelled() {
        return Err(cancelled());
    }
    progress.entries.fetch_add(1, Ordering::Relaxed);
    let context = Context {
        options: &options,
        progress: &progress,
        root_device: root_metadata.dev,
        next_directory: AtomicUsize::new(1),
        live,
    };
    let mut tree = Tree {
        root: root.clone(),
        nodes: Vec::new(),
        errors: Vec::new(),
        error_count: 0,
        elapsed_seconds: 0.0,
    };
    if let Some(sender) = live {
        sender
            .send(LiveEvent::Root(root.clone(), root_metadata.clone()))
            .map_err(|_| cancelled())?;
    }
    let mut records = Vec::new();
    if options.threads == 1 {
        // No pool or channel overhead when the caller requests serial scanning.
        let mut pending = vec![(root.clone(), 0)];
        while let Some((path, id)) = pending.pop() {
            if progress.is_cancelled() {
                return Err(cancelled());
            }
            let batch = read_directory(&path, id, &context);
            for entry in batch.entries.iter().rev() {
                if let Some(id) = entry.directory {
                    pending.push((path.join(&entry.name), id));
                }
            }
            collect_batch(batch, &mut records, &mut tree);
        }
    } else {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(options.threads)
            .build()
            .map_err(io::Error::other)?;
        // The coordinator waits outside the worker pool. The consumer drains
        // batches concurrently, so bounded result sends cannot starve workers.
        std::thread::scope(|threads| -> io::Result<()> {
            let (sender, receiver) = sync_channel(32);
            let context = &context;
            let root = root.clone();
            let coordinator = threads.spawn(move || {
                pool.scope(|scope| walk_parallel(root, 0, context, &sender, scope));
            });
            for batch in receiver {
                collect_batch(batch, &mut records, &mut tree);
            }
            coordinator
                .join()
                .map_err(|_| io::Error::other("scanner worker panicked"))
        })?;
    }
    if progress.is_cancelled() {
        return Err(cancelled());
    }
    progress.phase.store(1, Ordering::Relaxed);
    // Jobs may finish in any order. Build in sorted depth-first order afterward
    // so node IDs, hard-link attribution, and owner totals remain deterministic.
    let mut hard_links = HashSet::new();
    let root_name = root.file_name().unwrap_or(root.as_os_str()).to_os_string();
    append_node(&mut tree, root_name, root_metadata, None, &mut hard_links);
    let take_record =
        |records: &mut Vec<Option<Vec<Entry>>>, id: usize| -> io::Result<Vec<Entry>> {
            records
                .get_mut(id)
                .and_then(Option::take)
                .ok_or_else(|| io::Error::other("scanner did not return a directory result"))
        };
    let mut stack = vec![(0, take_record(&mut records, 0)?.into_iter())];
    while let Some((parent, entries)) = stack.last_mut() {
        if progress.is_cancelled() {
            return Err(cancelled());
        }
        if let Some(entry) = entries.next() {
            let id = append_node(
                &mut tree,
                entry.name,
                entry.metadata,
                Some(*parent),
                &mut hard_links,
            );
            if let Some(directory) = entry.directory {
                stack.push((id, take_record(&mut records, directory)?.into_iter()));
            }
        } else {
            stack.pop();
        }
    }
    tree.aggregate();
    if progress.is_cancelled() {
        return Err(cancelled());
    }
    tree.elapsed_seconds = start.elapsed().as_secs_f64();
    progress.phase.store(2, Ordering::Relaxed);
    Ok(tree)
}

// The terminal receives bounded deltas, not whole-tree clones. A parent's entries
// are sent before its child jobs can start, so directory IDs are available here.
pub(crate) enum LiveEvent {
    Root(PathBuf, EntryMetadata),
    Entries {
        parent: PathBuf,
        entries: Vec<(OsString, EntryMetadata)>,
    },
    Errors(u64, Vec<String>),
}

#[derive(Default)]
pub(crate) struct LiveState {
    directories: HashMap<PathBuf, usize>,
    hard_links: HashSet<(u64, u64)>,
    cached_index: Option<HashMap<(usize, OsString), usize>>,
    cached_links: HashMap<(u64, u64), usize>,
}

impl LiveState {
    pub fn from_cached(tree: &Tree) -> Self {
        let mut state = Self {
            cached_index: Some(HashMap::new()),
            ..Self::default()
        };
        for (id, node) in tree.nodes.iter().enumerate() {
            if node.is_dir {
                state.directories.insert(tree.path(id), id);
            } else if !node.shared_link {
                state.cached_links.entry(node.identity).or_insert(id);
            }
            if let Some(parent) = node.parent {
                state
                    .cached_index
                    .as_mut()
                    .unwrap()
                    .insert((parent, node.name.clone()), id);
            }
        }
        state
    }

    // Merge fresh metadata into the cached preview. Unvisited rows remain visibly
    // stale until the authoritative final tree removes missing entries.
    fn update_cached(&mut self, tree: &mut Tree, id: usize, metadata: EntryMetadata) {
        let node = &mut tree.nodes[id];
        let keep_children = node.is_dir && metadata.is_dir;
        let removed = if keep_children {
            HashMap::from([((node.uid, node.gid), node.own)])
        } else if node.is_dir {
            node.owners.clone()
        } else {
            HashMap::from([((node.uid, node.gid), node.total)])
        };
        let shared = !metadata.is_dir
            && metadata.nlink > 1
            && *self
                .cached_links
                .entry((metadata.dev, metadata.ino))
                .or_insert(id)
                != id;
        let own = if shared {
            Usage::default()
        } else {
            metadata.usage
        };
        let added = HashMap::from([((metadata.uid, metadata.gid), own)]);
        node.identity = (metadata.dev, metadata.ino);
        node.uid = metadata.uid;
        node.gid = metadata.gid;
        node.is_dir = metadata.is_dir;
        node.is_symlink = metadata.is_symlink;
        node.shared_link = shared;
        node.own = own;
        fn adjust(
            node: &mut Node,
            removed: &HashMap<(u32, u32), Usage>,
            added: &HashMap<(u32, u32), Usage>,
        ) {
            fn sub(value: &mut Usage, other: Usage) {
                value.allocated = value.allocated.saturating_sub(other.allocated);
                value.apparent = value.apparent.saturating_sub(other.apparent);
                value.inodes = value.inodes.saturating_sub(other.inodes);
            }
            for (&owner, &usage) in removed {
                sub(&mut node.total, usage);
                if node.is_dir {
                    sub(node.owners.entry(owner).or_default(), usage);
                }
            }
            for (&owner, &usage) in added {
                node.total.add(usage);
                if node.is_dir {
                    node.owners.entry(owner).or_default().add(usage);
                }
            }
        }
        if keep_children {
            adjust(node, &removed, &added);
        } else {
            node.children.clear();
            node.total = own;
            node.owners.clear();
            if node.is_dir {
                node.owners = added.clone();
            }
        }
        let mut parent = node.parent;
        while let Some(id) = parent {
            adjust(&mut tree.nodes[id], &removed, &added);
            parent = tree.nodes[id].parent;
        }
    }

    pub fn start(root: PathBuf, metadata: EntryMetadata) -> (Tree, Self) {
        let mut state = Self::default();
        state.directories.insert(root.clone(), 0);
        let name = root.file_name().unwrap_or(root.as_os_str()).to_os_string();
        let mut tree = Tree {
            root,
            nodes: vec![],
            errors: vec![],
            error_count: 0,
            elapsed_seconds: 0.0,
        };
        append_node(&mut tree, name, metadata, None, &mut state.hard_links);
        tree.aggregate();
        (tree, state)
    }

    pub fn apply(&mut self, tree: &mut Tree, event: LiveEvent) {
        match event {
            LiveEvent::Entries { parent, entries } => {
                let parent_id = self.directories[&parent];
                for (name, metadata) in entries {
                    if self.cached_index.is_some() {
                        let key = (parent_id, name.clone());
                        let id = match self.cached_index.as_ref().unwrap().get(&key).copied() {
                            Some(id) => id,
                            None => {
                                // A zero-contribution placeholder lets the common update path
                                // propagate the new entry exactly once.
                                let mut placeholder = metadata.clone();
                                placeholder.usage = Usage::default();
                                let id = append_node(
                                    tree,
                                    name,
                                    placeholder,
                                    Some(parent_id),
                                    &mut self.hard_links,
                                );
                                self.cached_index.as_mut().unwrap().insert(key, id);
                                id
                            }
                        };
                        if metadata.is_dir {
                            self.directories
                                .insert(parent.join(&tree.nodes[id].name), id);
                        }
                        self.update_cached(tree, id, metadata);
                        continue;
                    }
                    let path = metadata.is_dir.then(|| parent.join(&name));
                    let id =
                        append_node(tree, name, metadata, Some(parent_id), &mut self.hard_links);
                    let node = &mut tree.nodes[id];
                    let usage = node.own;
                    let owner = (node.uid, node.gid);
                    if let Some(path) = path {
                        self.directories.insert(path, id);
                        node.owners.insert(owner, usage);
                    }
                    // Update only ancestors, keeping all owner/filter views available.
                    let mut ancestor = Some(parent_id);
                    while let Some(id) = ancestor {
                        let node = &mut tree.nodes[id];
                        node.total.add(usage);
                        node.owners.entry(owner).or_default().add(usage);
                        ancestor = node.parent;
                    }
                }
            }
            LiveEvent::Errors(count, errors) => {
                tree.error_count += count;
                tree.errors.extend(
                    errors
                        .into_iter()
                        .take(MAX_ERRORS.saturating_sub(tree.errors.len())),
                );
            }
            LiveEvent::Root(_, metadata) => {
                assert!(
                    self.cached_index.is_some(),
                    "root is consumed before fresh live updates"
                );
                self.update_cached(tree, 0, metadata);
            }
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    #[test]
    fn live_batches_arrive_before_completion_and_reconcile_with_final_results() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/data"), [1; 4096]).unwrap();
        for i in 0..512 {
            fs::write(dir.path().join(format!("file-{i:04}")), [1; 16]).unwrap();
        }
        fs::write(dir.path().join("excluded"), b"excluded").unwrap();
        for threads in [1, 4] {
            let options = ScanOptions {
                path: dir.path().into(),
                threads,
                one_file_system: true,
                excludes: vec!["excluded".into()],
            };
            let expected = scan(options.clone(), Arc::default()).unwrap();
            let progress = Arc::new(Progress::default());
            let worker_progress = progress.clone();
            // Backpressure makes the before-completion assertion independent of timing.
            let (sender, receiver) = sync_channel(1);
            let worker = std::thread::spawn(move || scan_live(options, worker_progress, sender));
            let LiveEvent::Root(root, metadata) = receiver.recv().unwrap() else {
                panic!("root first");
            };
            let (mut live, mut state) = LiveState::start(root, metadata);
            state.apply(&mut live, receiver.recv().unwrap());
            assert!(live.nodes.len() > 1);
            assert!(live.nodes.len() < expected.nodes.len());
            assert_eq!(progress.phase.load(Ordering::Relaxed), 0);
            for event in receiver {
                state.apply(&mut live, event);
            }
            let final_tree = worker.join().unwrap().unwrap();
            assert_eq!(live.nodes[0].total, expected.nodes[0].total);
            assert_eq!(final_tree.nodes[0].total, expected.nodes[0].total);
            assert_eq!(live.nodes[0].owners, expected.nodes[0].owners);
            assert_eq!(live.error_count, final_tree.error_count);
            for id in 0..live.nodes.len() {
                let path = live.path(id);
                let final_id = (0..final_tree.nodes.len())
                    .find(|&i| final_tree.path(i) == path)
                    .unwrap();
                assert_eq!(live.nodes[id].total, final_tree.nodes[final_id].total);
                assert_eq!(live.nodes[id].owners, final_tree.nodes[final_id].owners);
            }
        }
    }

    #[test]
    fn cached_preview_updates_existing_rows_and_final_scan_removes_missing_entries() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("changed"), b"old").unwrap();
        fs::write(dir.path().join("gone"), b"stale").unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/data"), b"child").unwrap();
        let options = ScanOptions {
            path: dir.path().into(),
            threads: 2,
            one_file_system: false,
            excludes: vec![],
        };
        let mut cached = scan(options.clone(), Arc::default()).unwrap();
        let gone = cached.nodes.iter().find(|n| n.name == "gone").unwrap().own;
        let mut state = LiveState::from_cached(&cached);
        fs::write(dir.path().join("changed"), [1; 8192]).unwrap();
        fs::remove_file(dir.path().join("gone")).unwrap();
        fs::remove_file(dir.path().join("nested/data")).unwrap();
        fs::remove_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested"), b"now a file").unwrap();
        fs::write(dir.path().join("new"), b"new").unwrap();
        let (sender, receiver) = sync_channel(1);
        let worker = std::thread::spawn(move || scan_live(options, Arc::default(), sender));
        for event in receiver {
            state.apply(&mut cached, event);
        }
        let fresh = worker.join().unwrap().unwrap();
        assert_eq!(
            cached.nodes.iter().filter(|n| n.name == "changed").count(),
            1
        );
        assert_eq!(
            cached
                .nodes
                .iter()
                .find(|n| n.name == "changed")
                .unwrap()
                .own
                .apparent,
            8192
        );
        let mut expected_preview = fresh.nodes[0].total;
        expected_preview.add(gone);
        assert_eq!(cached.nodes[0].total, expected_preview);
        assert!(!fresh.nodes.iter().any(|n| n.name == "gone"));
        assert!(
            !cached
                .nodes
                .iter()
                .find(|n| n.name == "nested")
                .unwrap()
                .is_dir
        );
    }

    #[test]
    fn closing_live_receiver_cancels_blocked_scanner() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..256 {
            fs::write(dir.path().join(format!("{i}")), b"data").unwrap();
        }
        let options = ScanOptions {
            path: dir.path().into(),
            threads: 4,
            one_file_system: false,
            excludes: vec![],
        };
        let progress = Arc::new(Progress::default());
        let worker_progress = progress.clone();
        let (sender, receiver) = sync_channel(1);
        let worker = std::thread::spawn(move || scan_live(options, worker_progress, sender));
        receiver.recv().unwrap();
        drop(receiver);
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(progress.is_cancelled());
    }
}
