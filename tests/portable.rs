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
        .arg("--cache-db")
        .arg(dir.path().join("unused.sqlite3"))
        .arg(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !dir.path().join("unused.sqlite3").exists(),
        "JSON must bypass caching"
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

#[test]
fn json_human_readable_adds_sizes_without_changing_numeric_report() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("data"), [1u8; 8192]).unwrap();
    fs::write(dir.path().join("empty"), []).unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    fs::write(dir.path().join("nested/child"), [2u8; 1024]).unwrap();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_zhandu"))
            .arg(dir.path())
            .arg("--json")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    fn check_and_strip(value: &mut serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(fields) => {
                let mut count = 0;
                if let Some(bytes) = fields.get("allocated").and_then(|v| v.as_u64()) {
                    assert_eq!(
                        fields.remove("allocated_human").unwrap(),
                        zhandu::model::human(bytes)
                    );
                    let bytes = fields["apparent"].as_u64().unwrap();
                    assert_eq!(
                        fields.remove("apparent_human").unwrap(),
                        zhandu::model::human(bytes)
                    );
                    assert!(fields["inodes"].is_u64());
                    count += 1;
                }
                count + fields.values_mut().map(check_and_strip).sum::<usize>()
            }
            serde_json::Value::Array(rows) => rows.iter_mut().map(check_and_strip).sum(),
            _ => 0,
        }
    }
    let first = run(&[]);
    let owner = &first["users"][0];
    let identity = owner["sid"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| owner["id"].as_u64().unwrap().to_string());
    for args in [vec![], vec!["--apparent-size"], vec!["--user", &identity]] {
        let mut plain = run(&args);
        assert!(!plain.to_string().contains("_human"));
        let mut formatted_args = args.clone();
        formatted_args.push("--human-readable");
        let mut formatted = run(&formatted_args);
        let expected = 2
            + formatted["groups"].as_array().unwrap().len()
            + formatted["users"].as_array().unwrap().len()
            + formatted["children"].as_array().unwrap().len();
        assert_eq!(check_and_strip(&mut formatted), expected);
        plain.as_object_mut().unwrap().remove("elapsed_seconds");
        formatted.as_object_mut().unwrap().remove("elapsed_seconds");
        assert_eq!(formatted, plain);
    }
    let invalid = Command::new(env!("CARGO_BIN_EXE_zhandu"))
        .arg("--human-readable")
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("--json"));
}
