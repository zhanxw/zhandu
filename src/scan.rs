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
        for (name, result) in chunk {
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
    };
    let mut tree = Tree {
        root: root.clone(),
        nodes: Vec::new(),
        errors: Vec::new(),
        error_count: 0,
        elapsed_seconds: 0.0,
    };
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
