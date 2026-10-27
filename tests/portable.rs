use std::{fs, process::Command, sync::Arc};
use zhandu::{
    model::Filter,
    scan::{ScanOptions, scan},
};

#[test]
fn native_scan_deduplicates_hardlinks_and_preserves_owners() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    fs::write(dir.path().join("nested/data"), [42u8; 8192]).unwrap();
    fs::hard_link(dir.path().join("nested/data"), dir.path().join("linked")).unwrap();
    let mut snapshots = Vec::new();
    for threads in [1, 4] {
        let tree = scan(
            ScanOptions {
                path: dir.path().into(),
                threads,
                one_file_system: true,
                excludes: vec![],
            },
            Arc::default(),
        )
        .unwrap();
        assert_eq!(tree.error_count, 0, "{:?}", tree.errors);
        assert_eq!(tree.nodes.iter().filter(|n| n.shared_link).count(), 1);
        assert_eq!(tree.nodes[0].total.inodes, 3);
        assert!(tree.nodes[0].total.apparent >= 8192);
        assert_eq!(
            tree.ownership(0, false, Filter::default())
                .iter()
                .map(|(_, u)| u.inodes)
                .sum::<u64>(),
            3
        );
        snapshots.push(tree.nodes[0].total);
    }
    assert_eq!(snapshots[0], snapshots[1]);
}

#[test]
fn native_cli_reports_usage_and_rejects_missing_roots() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("data"), [1u8; 4096]).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zhandu"))
        .arg("--json")
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["complete"], true);
    assert!(report["total"]["apparent"].as_u64().unwrap() >= 4096);
    assert_eq!(report["total"]["inodes"], 2);
    #[cfg(windows)]
    {
        for (option, field) in [("--user", "users"), ("--group", "groups"), ("-g", "groups")] {
            let sid = report[field][0]["sid"]
                .as_str()
                .expect("Windows JSON must identify owners by SID");
            assert!(sid.starts_with("S-"));
            let filtered = Command::new(env!("CARGO_BIN_EXE_zhandu"))
                .args(["--json", option, sid])
                .arg(dir.path())
                .output()
                .unwrap();
            assert!(
                filtered.status.success(),
                "{}",
                String::from_utf8_lossy(&filtered.stderr)
            );
            let filtered: serde_json::Value = serde_json::from_slice(&filtered.stdout).unwrap();
            assert_eq!(filtered["total"], report["total"]);
            assert_eq!(filtered[field][0]["sid"], sid);
        }
    }
    let output = Command::new(env!("CARGO_BIN_EXE_zhandu"))
        .arg("--json")
        .arg(dir.path().join("missing"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
}

#[cfg(windows)]
#[test]
fn windows_display_escapes_control_characters_and_unpaired_surrogates() {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    let name = OsString::from_wide(&[b'a' as u16, 0xd800, 27, b'z' as u16]);
    let rendered = zhandu::model::display_name(&name);
    assert_eq!(rendered, "a\\u{d800}\\u{1b}z");
}
