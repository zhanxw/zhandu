# zhandu

A fast, read-only disk usage explorer written in Rust for **Windows, Linux, and macOS**. Scan recursively, browse with arrow keys, and compare usage by owning group or user.

[Download the latest release](https://github.com/zhanxw/zhandu/releases/latest) · [Build status](https://github.com/zhanxw/zhandu/actions/workflows/ci.yml)

## Run

Download and extract the archive for your platform, then run `zhandu` (`zhandu.exe` in Windows PowerShell). Release archives contain documentation, licenses, and an optimized executable. Verify downloads against the release’s `SHA256SUMS`.

For a local source build, the executable is `target/release/zhandu` (`target/release/zhandu.exe` on Windows).

```sh
# From the project directory: scan the current directory
./target/release/zhandu

# Browse a particular directory, starting with group totals
./target/release/zhandu /shared/project --view groups

# Filter by group or user (names or numeric IDs)
./target/release/zhandu /shared/project --group research
./target/release/zhandu /shared/project --group 1001 --user 1002

# Reduce parallelism on shared storage; stay within one filesystem
./target/release/zhandu /shared/project --threads 2 -x

# Exact basename exclusions, applied at every depth
./target/release/zhandu . --exclude target --exclude .git

# Noninteractive, machine-readable totals
./target/release/zhandu /shared/project --json
```

Use the binary's absolute path from another directory to scan that directory, or install it on your PATH with `cargo install --path . --locked` from this project. No global installation was performed during development.

## Build

Requires Rust 1.88 or newer. Native Windows (MSVC), Linux, and macOS builds are supported. Cargo.lock pins the dependency versions used for this build.

```sh
cargo build --release --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Use the release build for performance measurements. The first dependency download requires internet access; subsequent builds can use `--offline`.

## Keyboard controls

| Key | Action |
| --- | --- |
| Up / Down, or k / j | Move selection |
| Right / Enter | From owner panel: focus directory panel; from directory panel: open selected directory |
| Left / Backspace, or h | Parent directory; at the owner view’s starting path, return focus to owners |
| Tab / Shift-Tab | Switch owner/directory panel focus; Tab from the full directory view opens groups |
| g / u / d | Choose group / choose user / directory browser |
| Escape | Clear both ownership filters; close help/error panel when open |
| a | Toggle allocated disk usage / apparent size |
| s | Toggle size / name sorting |
| Home / End, PageUp / PageDown | Navigate long lists |
| e | Inspect scan errors; up/down scroll |
| ? | Help |
| q | Quit |
| Ctrl-C | Cancel / interrupt |

Press **g** or **u** to choose an owner. The selector shows a **subdirectory usage preview** for the highlighted group or user; up/down changes the preview. Press **Right** or **Enter** to move the cursor into the directory panel. Up/down selects a directory there; Right/Enter opens it and Left/Backspace returns to its parent. **Tab** switches focus back to the owner list without changing the current path. The focused panel has a cyan border. Press **d** for a full file-and-directory listing with the selected filter. Each subdirectory size includes all matching descendants, regardless of the directory's own owner. Directories with zero matching usage remain visible.

The selector appears beside the preview in wide terminals and above it in narrower terminals. Both panels scroll independently. The owner list and its totals stay anchored to the directory where you opened the selector, so the selected owner stays available even when a visited subdirectory has no matching usage. At that starting directory, Left returns focus to the owner list. A selected group and user filter combine with AND. Reopening g or u lists alternatives to the current owner while retaining the other dimension's filter. Escape clears both filters. Names resolve asynchronously. Unix numeric IDs and Windows SID strings remain visible if lookups fail. `--numeric-ids` disables display-name lookups (explicit command-line names still require resolution).

The initial screen reports scan progress and supports cancellation. Browsing opens once the scan and aggregation finish. Subsequent navigation and ownership filtering use the in-memory tree and do not rescan the disk.

## Accounting rules

- **On Unix, group means each file's owning GID and user means its UID.** On Windows, these are the file security descriptor’s primary-group SID and owner SID, not the groups in its access-control list. Supplementary group memberships do not duplicate usage. A directory's ownership does not determine the ownership of its descendants.
- Allocated usage is Unix metadata block count × 512 bytes, or Windows `FILE_STANDARD_INFO.AllocationSize`. Apparent usage is logical length, including directory and symlink lengths. Sparse files can have much larger apparent sizes.
- Hidden files and directories are included. Git ignores are not applied. Symlinks within the tree are counted but never followed; an explicit root symlink resolves to its target. Windows reparse points, including junctions, are also not traversed.
- Hard-linked non-directory inodes are counted once by `(device, inode)` on Unix or `(volume serial, file index)` on Windows, attributed to the first path in sorted depth-first traversal. Additional paths show a hard-link marker and zero attributed bytes. A subtree can therefore have a different total when scanned independently. Shared blocks from APFS clones/reflinks and snapshots are not deduplicated.
- Directory metadata blocks belong to that directory's UID/GID. `[directory itself]` makes the directory view reconcile: its own usage plus child totals equals the current subtree total.
- “Inodes” counts attributed objects, including directories and symlinks. Duplicate hard-link paths contribute zero. Zero-attribution files are omitted when an owner filter is active; directories remain visible.
- `-x` excludes entries on other devices, including those mountpoint entries. `--exclude NAME` skips exact basenames and their descendants; it is not a glob or a relative-path pattern.
- Filesystem scans are not atomic snapshots. Unreadable/disappearing paths produce **INCOMPLETE** results, an error count, and up to 100 error details. Explicit exclusions are intentional scope limits, not errors.
- Allocated totals are file-attributed estimates, not a promise of reclaimable physical space or a replacement for filesystem quotas / `df`.

## JSON and exit codes

`--json` emits schema version 1 with both allocated and apparent bytes, unique inode counts, per-group/per-user totals, immediate child summaries, directory self-usage, active filters, elapsed time, and completeness/errors. `--apparent-size` changes sorting; both byte measures remain in JSON. Unix ownership records use numeric IDs. Windows records additionally contain a persistent `sid` string; their numeric `id` values are process-local intern keys and must not be compared across runs. Windows command-line filters accept account names (for example `DOMAIN\user`) or SID strings (`S-1-...`), rather than Unix-style numeric IDs. Names and paths are escaped display strings; do not treat them as unescaped filesystem paths. The export is a summary, not a persistent scan cache.

| Code | Meaning |
| --- | --- |
| 0 | Scan completed without reported filesystem errors |
| 1 | Fatal error |
| 2 | Partial scan, or command-line usage error (Clap) |
| 130 | Cancelled or interrupted |

When piping output, use `--json`; interactive mode requires terminal input and output.

## Performance

Directory traversal and file metadata retrieval share a fixed-size Rayon work-stealing pool. Large directories use 64-entry metadata batches; small directories avoid extra scheduling overhead. The default is the available CPU count capped at eight workers, configurable with `--threads` (1–256). Compact metadata records, batched progress counters, and deterministic bottom-up ownership aggregation preserve accurate group/user filtering.

After studying parallel-disk-usage, the scheduler was changed to handle flat folders efficiently. On this macOS ARM64 machine, paired seven-run warm-cache benchmarks with 30,000 files and eight workers measured:

| Layout | Before | After | Before / after | System du |
| --- | ---: | ---: | ---: | ---: |
| One folder, 30,000 files | 122.9 ms | 58.8 ms | 2.09× | 47.3 ms |
| 300 sibling folders × 100 files | 41.9 ms | 42.5 ms | 0.99× | 47.3 ms |
| 80 nested folders × 375 files | 218.1 ms | 85.8 ms | 2.54× | 43.1 ms |

Before/after ownership reports matched, and allocated totals matched system `du`. These are local workload-specific measurements. See [PERFORMANCE.md](PERFORMANCE.md) for source references, memory tradeoffs, exact methodology, and baseline comparison commands.

Reproduce with Python 3:

```sh
python3 scripts/benchmark.py target/release/zhandu
```

The script creates and removes its own temporary fixture, verifies totals against `du`, and saves timings and memory measurements to `benchmark-results/local.json`. It does not flush system caches. Memory grows with file count and directory/owner combinations; disk-backed indexing and scan caching are not implemented.

## Verification and platform status

Local validation and native CI coverage:

- Rust tests: sparse files, hard links across directories, symlinks/loops, hidden files, exclusions, denied directories, cancellation, deep trees, serial/parallel agreement, CLI output, ownership accounting, navigation, and tiny terminal sizes.
- A real second-group fixture using an existing permitted supplementary group; synthetic metadata also covers different UIDs and combined UID/GID filters.
- Real PTY checks for keyboard navigation, resizing, normal quit, fatal scan errors, SIGINT, scan cancellation, and terminal-mode restoration.
- Formatting and Clippy with warnings denied; optimized release build.

```sh
python3 scripts/pty_smoke.py target/release/zhandu
```

The GitHub Actions workflow builds and tests five native targets: Windows x64, Linux x64/ARM64, and macOS Intel/Apple Silicon. See the linked Actions run for current platform results. Windows has dedicated native metadata, hard-link, SID filter, and filename-escaping tests; terminal rendering/navigation is exercised with Ratatui’s test backend. Real PTY tests run on Linux/macOS. Real multi-group and permission-denial tests explicitly report a skip when their host permissions cannot exercise the case. Linux-specific non-UTF-8 filename coverage is also configured; APFS rejects such filenames, so escaping is additionally unit-tested without filesystem creation. Cross-device mount exclusion has not been exercised against a mounted integration fixture.

## Automated releases

The [build workflow](.github/workflows/ci.yml) runs on main-branch pushes, pull requests, and manual dispatch. Push a version tag matching Cargo.toml (for example `v0.1.0`) to run all five native test/build jobs and publish archives automatically. Publishing waits for every build to succeed, validates the complete archive set, uploads SHA-256 checksums, and only then makes the release public. A published version is never overwritten by a rerun.

Linux x64 builds target glibc 2.35+; Linux ARM64 targets glibc 2.39+. Windows uses MSVC on Windows Server 2022; NTFS is the primary validated filesystem. Metadata/security access failures are reported as incomplete scans. ReFS/FAT/network-share behavior is not validated. Archives are not notarized or Authenticode-signed.

## Layout

- `src/scan.rs`: parallel metadata traversal, deduplication, progress/errors.
- `src/platform.rs` and `src/platform/windows.rs`: Unix metadata and Windows native file/security APIs.
- `src/model.rs`: tree, ownership aggregation, filtering, terminal-safe display.
- `src/ui.rs`: TUI, navigation, asynchronous account-name resolution.
- `src/main.rs`: command line, JSON reports, exit handling.
- `tests/accounting.rs`: filesystem and command-line integration tests.
- `scripts/`: real-terminal smoke tests and reproducible benchmark.

MIT licensed. Dependencies retain their own licenses.
