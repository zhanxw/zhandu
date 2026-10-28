//! Background scan lifecycle, shared by startup and manual refresh.
use crate::{
    cache::{Cache, CacheConfig},
    deletion::Mark,
    model::Tree,
    scan::{LiveEvent, Progress, ScanOptions, scan_live},
};
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
};

type Startup = (Option<(Tree, i64)>, Option<String>);
pub(crate) type Completed = (io::Result<Tree>, Option<String>, Vec<PathBuf>);
pub(crate) struct ScanSession {
    pub startup: Receiver<Startup>,
    pub events: Receiver<LiveEvent>,
    pub completed: Receiver<Completed>,
}
impl ScanSession {
    pub fn start(
        options: ScanOptions,
        progress: Arc<Progress>,
        config: CacheConfig,
        load_cache: bool,
        marks: Vec<Mark>,
    ) -> Self {
        let (startup_tx, startup) = mpsc::channel();
        let (live_tx, events) = mpsc::sync_channel(32);
        let (done_tx, completed) = mpsc::channel();
        std::thread::spawn(move || {
            let (cache, mut warning) = match Cache::new(&options, &config) {
                Ok(cache) => (cache, None),
                Err(error) => (None, Some(format!("Cache unavailable: {error}"))),
            };
            let cached = if load_cache {
                match cache.as_ref().map(|c| c.load(&progress)).transpose() {
                    Ok(cached) => cached.flatten(),
                    Err(error) => {
                        warning = Some(format!("Cache ignored: {error}"));
                        None
                    }
                }
            } else {
                None
            };
            if startup_tx.send((cached, warning.clone())).is_err() {
                return;
            }
            let result = scan_live(options, progress.clone(), live_tx);
            let message = if let (Some(cache), Ok(tree)) = (&cache, &result) {
                Some(match cache.save(tree, &progress) {
                    Ok(()) => {
                        if tree.error_count > 0 {
                            "Partial cache saved; previous complete snapshot retained".into()
                        } else {
                            "Cache saved".into()
                        }
                    }
                    Err(error) => format!("Cache not saved: {error}"),
                })
            } else {
                warning
            };
            // Never silently re-arm a changed file with its new identity/content.
            let valid = if result.is_ok() {
                marks
                    .into_iter()
                    .filter(|m| !progress.is_cancelled() && m.revalidate().is_ok())
                    .map(|m| m.path)
                    .collect()
            } else {
                vec![]
            };
            let _ = done_tx.send((result, message, valid));
        });
        Self {
            startup,
            events,
            completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{deletion::Mark, scan::scan};
    #[test]
    fn refresh_revalidates_marks_and_reports_failure_without_losing_the_ui() {
        let dir = tempfile::tempdir().unwrap();
        let unchanged = dir.path().join("keep");
        let changed = dir.path().join("change");
        std::fs::write(&unchanged, b"same").unwrap();
        std::fs::write(&changed, b"old").unwrap();
        let options = ScanOptions {
            path: dir.path().into(),
            threads: 2,
            one_file_system: false,
            excludes: vec![],
        };
        let tree = scan(options.clone(), Arc::default()).unwrap();
        let marks: Vec<_> = tree
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| !n.is_dir)
            .map(|(id, _)| Mark::new(&tree, id).unwrap())
            .collect();
        std::fs::write(&changed, b"different content").unwrap();
        let config = CacheConfig {
            enabled: false,
            path: None,
        };
        let session = ScanSession::start(
            options.clone(),
            Arc::default(),
            config.clone(),
            false,
            marks,
        );
        for _ in &session.events {}
        let (result, _, valid) = session.completed.recv().unwrap();
        assert!(result.is_ok());
        assert_eq!(valid, vec![unchanged.canonicalize().unwrap()]);
        let session = ScanSession::start(
            ScanOptions {
                path: dir.path().join("missing"),
                ..options
            },
            Arc::default(),
            config,
            false,
            vec![],
        );
        let (result, _, valid) = session.completed.recv().unwrap();
        assert!(result.is_err());
        assert!(valid.is_empty());
    }
}
