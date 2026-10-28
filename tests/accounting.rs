#![cfg(unix)]

use std::{
    collections::HashSet,
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
    sync::Arc,
};
use zhandu::{
    model::{Filter, Usage},
    scan::{Progress, ScanOptions, scan},
};

fn options(path: &Path, threads: usize) -> ScanOptions {
    ScanOptions {
        path: path.into(),
        threads,
        one_file_system: false,
        excludes: vec![],
    }
}

// Independent iterative metadata oracle: sums each inode once, including directories.
fn expected(root: &Path) -> Usage {
    let mut stack = vec![root.to_path_buf()];
    let mut seen = HashSet::new();
    let mut total = Usage::default();
    while let Some(path) = stack.pop() {
        let metadata = fs::symlink_metadata(&path).unwrap();
        if seen.insert((metadata.dev(), metadata.ino())) {
            total.add(Usage {
                allocated: metadata.blocks() * 512,
                apparent: metadata.len(),
                inodes: 1,
            });
        }
        if metadata.is_dir() {
            stack.extend(fs::read_dir(path).unwrap().map(|e| e.unwrap().path()));
        }
    }
    total
}

#[test]
fn sparse_hardlinks_hidden_symlinks_and_parallel_consistency() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("a")).unwrap();
    fs::create_dir(root.join("b")).unwrap();
    fs::write(root.join("a/data"), [42u8; 32768]).unwrap();
    fs::hard_link(root.join("a/data"), root.join("b/linked")).unwrap();
    fs::File::create(root.join("sparse"))
        .unwrap()
        .set_len(128 * 1024 * 1024)
        .unwrap();
    fs::write(root.join(".hidden"), b"hidden").unwrap();
    fs::write(root.join("control\x1b\n"), b"bytes").unwrap();
    // APFS rejects invalid UTF-8 names; exercise those on Linux filesystems.
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStringExt;
        fs::write(
            root.join(std::ffi::OsString::from_vec(b"bad\xff".to_vec())),
            b"bytes",
        )
        .unwrap();
    }
    symlink("../", root.join("a/loop")).unwrap();
    symlink("missing", root.join("broken")).unwrap();
    let serial = scan(options(root, 1), Arc::default()).unwrap();
    let parallel = scan(options(root, 4), Arc::default()).unwrap();
    assert_eq!(serial.error_count, 0);
    assert_eq!(serial.nodes[0].total, expected(root));
    assert_eq!(serial.nodes[0].total, parallel.nodes[0].total);
    assert_eq!(serial.nodes.len(), parallel.nodes.len());
    for (a, b) in serial.nodes.iter().zip(&parallel.nodes) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.own, b.own);
    }
    assert_eq!(parallel.nodes.iter().filter(|n| n.shared_link).count(), 1);
    assert!(parallel.nodes[0].total.apparent > parallel.nodes[0].total.allocated);
    for by_group in [false, true] {
        let mut partition = Usage::default();
        for (_, usage) in parallel.ownership(0, by_group, Filter::default()) {
            partition.add(usage);
        }
        assert_eq!(partition, parallel.nodes[0].total);
    }
    // Each directory reconciles to its own inode plus immediate children's subtree totals.
    for node in parallel.nodes.iter().filter(|n| n.is_dir) {
        let mut sum = node.own;
        for &child in &node.children {
            sum.add(parallel.nodes[child].total);
        }
        assert_eq!(sum, node.total);
    }
}

#[test]
fn wide_directory_parallel_metadata_preserves_order_links_and_progress() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..1024 {
        fs::File::create(dir.path().join(format!("file{index:04}"))).unwrap();
    }
    fs::write(dir.path().join("file0000"), [42u8; 4096]).unwrap();
    fs::hard_link(dir.path().join("file0000"), dir.path().join("linked")).unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    fs::write(dir.path().join("nested/data"), [7u8; 8192]).unwrap();
    symlink("nested", dir.path().join("symlink")).unwrap();
    let serial = scan(options(dir.path(), 1), Arc::default()).unwrap();
    let progress = Arc::new(Progress::default());
    let parallel = scan(options(dir.path(), 4), progress.clone()).unwrap();
    assert_eq!(serial.nodes[0].total, expected(dir.path()));
    assert_eq!(serial.nodes.len(), parallel.nodes.len());
    assert_eq!(parallel.error_count, 0);
    assert_eq!(
        progress.entries.load(std::sync::atomic::Ordering::Relaxed),
        parallel.nodes.len() as u64
    );
    for (a, b) in serial.nodes.iter().zip(&parallel.nodes) {
        assert_eq!(a.name, b.name);
        assert_eq!(a.parent, b.parent);
        assert_eq!(a.own, b.own);
        assert_eq!(a.total, b.total);
        assert_eq!(a.owners, b.owners);
        assert_eq!(a.shared_link, b.shared_link);
    }
}

#[test]
fn exclusions_prune_subtrees_and_root_symlink_resolves() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("root");
    fs::create_dir_all(root.join("skip/deep")).unwrap();
    fs::write(root.join("skip/deep/file"), b"excluded").unwrap();
    fs::write(root.join("keep"), b"included").unwrap();
    symlink(&root, temp.path().join("alias")).unwrap();
    let mut config = options(&temp.path().join("alias"), 2);
    config.excludes = vec!["skip".into()];
    let tree = scan(config, Arc::default()).unwrap();
    assert_eq!(tree.nodes.len(), 2);
    assert_eq!(tree.root, root.canonicalize().unwrap());
    assert_eq!(tree.nodes[1].name, "keep");
}

#[test]
fn denied_directory_marks_scan_incomplete() {
    let temp = tempfile::tempdir().unwrap();
    let denied = temp.path().join("denied");
    fs::create_dir(&denied).unwrap();
    fs::write(denied.join("secret"), b"test").unwrap();
    fs::create_dir(temp.path().join("readable")).unwrap();
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o000)).unwrap();
    let denied_to_process = fs::read_dir(&denied).is_err();
    let result = scan(options(temp.path(), 2), Arc::default());
    fs::set_permissions(&denied, fs::Permissions::from_mode(0o700)).unwrap();
    let tree = result.unwrap();
    if denied_to_process {
        assert!(tree.error_count > 0);
        assert!(!tree.errors.is_empty());
        assert_eq!(tree.nodes[0].scan.errors, tree.error_count);
        assert_eq!(
            tree.nodes
                .iter()
                .find(|n| n.name == "denied")
                .unwrap()
                .scan
                .label(),
            "incomplete"
        );
        assert_eq!(
            tree.nodes
                .iter()
                .find(|n| n.name == "readable")
                .unwrap()
                .scan
                .label(),
            "complete"
        );
    } else {
        eprintln!("permission-denial assertion skipped: process can read mode-000 directory");
    }
}

#[test]
fn cancellation_and_invalid_root_are_errors() {
    let temp = tempfile::tempdir().unwrap();
    let progress = Arc::new(Progress::default());
    progress.cancel();
    assert_eq!(
        scan(options(temp.path(), 2), progress).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert!(scan(options(&temp.path().join("missing"), 1), Arc::default()).is_err());
    fs::write(temp.path().join("file"), b"data").unwrap();
    assert_eq!(
        scan(options(&temp.path().join("file"), 1), Arc::default())
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn cli_json_has_reconciling_totals_and_filters() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(temp.path().join("data"), [0; 8192]).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_zhandu"))
        .args(["--json", "--threads", "2"])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["complete"], true);
    assert_eq!(
        report["total"]["allocated"],
        expected(temp.path()).allocated
    );
    let gid = fs::metadata(temp.path()).unwrap().gid();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_zhandu"))
        .args(["--json", "--group", &gid.to_string()])
        .arg(temp.path())
        .output()
        .unwrap();
    let filtered: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(filtered["total"], report["total"]);
    let group_name = uzers::get_group_by_gid(gid)
        .unwrap()
        .name()
        .to_string_lossy()
        .into_owned();
    for group in [gid.to_string(), group_name, gid.wrapping_add(1).to_string()] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_zhandu"))
            .args(["--json", "-g", &group])
            .arg(temp.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let short: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        if group == gid.wrapping_add(1).to_string() {
            assert_eq!(short["total"]["inodes"], 0);
            assert_eq!(short["total"]["allocated"], 0);
        } else {
            assert_eq!(short["total"], filtered["total"]);
        }
    }

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_zhandu"))
        .args(["--threads", "0", "--json"])
        .arg(temp.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
}

#[test]
fn deep_directory_tree_has_correct_parent_links() {
    let temp = tempfile::tempdir().unwrap();
    let mut leaf = temp.path().to_owned();
    for _ in 0..100 {
        leaf.push("d");
        fs::create_dir(&leaf).unwrap();
    }
    fs::write(leaf.join("file"), b"deep").unwrap();
    let tree = scan(options(temp.path(), 4), Arc::default()).unwrap();
    assert_eq!(tree.nodes.len(), 102);
    assert_eq!(tree.path(101), leaf.join("file").canonicalize().unwrap());
    assert_eq!(tree.nodes[0].total, expected(temp.path()));
}

#[test]
fn actual_group_ownership_is_partitioned_when_chgrp_is_allowed() {
    let temp = tempfile::tempdir().unwrap();
    let primary_gid = fs::metadata(temp.path()).unwrap().gid();
    let file = temp.path().join("other-group");
    fs::write(&file, [7; 8192]).unwrap();
    let alternate = uzers::get_current_username()
        .and_then(|name| uzers::get_user_groups(&name, primary_gid))
        .unwrap_or_default()
        .iter()
        .map(|g| g.gid())
        .find(|&gid| {
            gid != primary_gid && std::os::unix::fs::chown(&file, None, Some(gid)).is_ok()
        });
    let Some(alternate_gid) = alternate else {
        eprintln!(
            "real multi-group fixture skipped: no permitted alternate group; synthetic ownership tests still run"
        );
        return;
    };
    let tree = scan(options(temp.path(), 2), Arc::default()).unwrap();
    let groups = tree.ownership(0, true, Filter::default());
    assert_eq!(groups.len(), 2);
    let other = tree.nodes[0].usage(Filter {
        uid: None,
        gid: Some(alternate_gid),
    });
    assert_eq!(other.allocated, fs::metadata(&file).unwrap().blocks() * 512);
    assert_eq!(other.inodes, 1);
    let original = tree.nodes[0].usage(Filter {
        uid: None,
        gid: Some(primary_gid),
    });
    assert_eq!(original.inodes, 1); // the parent directory retains its own group
}
